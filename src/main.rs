use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

// ── Schema types ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct HeapMeta {
    node_fields: Vec<String>,
    node_types: Vec<Value>,
    edge_fields: Vec<String>,
    edge_types: Vec<Value>,
}

#[derive(Deserialize)]
struct SnapshotSection {
    meta: HeapMeta,
}

/// Top-level .heapsnapshot structure.
/// serde_json will skip unknown fields (trace_tree, samples, locations…)
/// without allocating memory for them.
#[derive(Deserialize)]
struct HeapSnapshot {
    snapshot: SnapshotSection,
    nodes: Vec<u32>,
    edges: Vec<u32>,
    strings: Vec<String>,
}

// ── Stats accumulator ────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct CtorStats {
    count: u64,
    shallow: u64,
    det_count: u64,
    det_size: u64,
}

// ── Formatting helpers ───────────────────────────────────────────────────────

fn fmt_bytes(b: u64) -> String {
    if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.0} kB", b as f64 / 1024.0)
    } else {
        format!("{} B", b)
    }
}

fn pct(n: u64, total: u64) -> String {
    if total == 0 {
        return "0.0%".to_string();
    }
    format!("{:.1}%", (n as f64 / total as f64) * 100.0)
}

fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Main analysis ────────────────────────────────────────────────────────────

fn analyze(snap: &HeapSnapshot, file_path: &str, file_size: u64) -> String {
    let meta = &snap.snapshot.meta;
    let node_fields = &meta.node_fields;
    let edge_fields = &meta.edge_fields;

    // Extract enum arrays from the heterogeneous node_types / edge_types
    let node_types: Vec<&str> = meta
        .node_types
        .first()
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let edge_types: Vec<&str> = meta
        .edge_types
        .first()
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let nfc = node_fields.len(); // node field count (usually 7)
    let efc = edge_fields.len(); // edge field count (usually 3)

    let nf_type = node_fields.iter().position(|f| f == "type").unwrap_or(0);
    let nf_name = node_fields.iter().position(|f| f == "name").unwrap_or(1);
    let nf_self_size = node_fields.iter().position(|f| f == "self_size").unwrap_or(3);
    let nf_edge_count = node_fields.iter().position(|f| f == "edge_count").unwrap_or(4);
    let nf_detached = node_fields.iter().position(|f| f == "detachedness");

    let ef_type = edge_fields.iter().position(|f| f == "type").unwrap_or(0);
    let ef_to = edge_fields.iter().position(|f| f == "to_node").unwrap_or(2);

    let nodes = &snap.nodes;
    let edges = &snap.edges;
    let strings = &snap.strings;

    let node_count = nodes.len() / nfc;
    let edge_count = edges.len() / efc;

    eprintln!("Nodes: {node_count}, Edges: {edge_count}");

    // ── Build per-node edge start index ─────────────────────────────────────
    eprintln!("Building edge offset index…");
    let mut edge_start = vec![0u32; node_count + 1];
    let mut ep: u32 = 0;
    for i in 0..node_count {
        edge_start[i] = ep;
        ep = ep.saturating_add(nodes[i * nfc + nf_edge_count]);
    }
    edge_start[node_count] = ep;

    // ── Single pass: aggregate constructors + detached DOM ───────────────────
    eprintln!("Aggregating constructors…");
    let mut ctor_map: HashMap<String, CtorStats> = HashMap::with_capacity(8192);
    let mut detached_map: HashMap<String, (u64, u64)> = HashMap::new();
    let mut total_det_size: u64 = 0;

    for i in 0..node_count {
        let base = i * nfc;
        let type_idx = nodes[base + nf_type] as usize;
        let name_idx = nodes[base + nf_name] as usize;
        let self_size = nodes[base + nf_self_size] as u64;
        let type_name = node_types.get(type_idx).copied().unwrap_or("?");
        let name_str = strings.get(name_idx).map(|s| s.as_str()).unwrap_or("");
        let is_det = nf_detached
            .map(|nfd| nodes[base + nfd] == 2)
            .unwrap_or(false);

        // Constructor label: use object name for object/closure/native/regexp,
        // else use the type in parens.
        let ctor_name: String = match type_name {
            "object" | "closure" | "native" | "regexp" => {
                if name_str.is_empty() {
                    format!("({type_name})")
                } else {
                    name_str.to_string()
                }
            }
            _ => format!("({type_name})"),
        };

        let entry = ctor_map.entry(ctor_name.clone()).or_default();
        entry.count += 1;
        entry.shallow += self_size;

        if is_det {
            entry.det_count += 1;
            entry.det_size += self_size;

            let det_label = if name_str.is_empty() {
                format!("Detached ({type_name})")
            } else {
                format!("Detached {name_str}")
            };
            let e = detached_map.entry(det_label).or_insert((0, 0));
            e.0 += 1;
            e.1 += self_size;
            total_det_size += self_size;
        }
    }

    // Sort constructors
    let mut by_size: Vec<(&String, &CtorStats)> = ctor_map.iter().collect();
    by_size.sort_by(|a, b| b.1.shallow.cmp(&a.1.shallow));

    let mut by_count: Vec<(&String, &CtorStats)> = ctor_map.iter().collect();
    by_count.sort_by(|a, b| b.1.count.cmp(&a.1.count));

    let total_size: u64 = by_size.iter().map(|(_, v)| v.shallow).sum();

    let mut det_sorted: Vec<(&String, &(u64, u64))> = detached_map.iter().collect();
    det_sorted.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));

    // ── Suspicious / framework patterns ─────────────────────────────────────
    const SUSPICIOUS_KWS: &[&str] = &[
        "vnode", "VNode", "vue", "Vue", "Component", "component",
        "Watcher", "watcher", "Effect", "effect", "Reactive", "reactive",
        "Proxy", "proxy", "Ref", "Store", "store", "Router", "router",
        "EventListener", "EventEmitter", "Subscription", "subscription",
        "Observer", "Cache", "cache", "Registry", "registry",
        "Tooltip", "Modal", "modal", "Dropdown", "Portal", "Teleport",
    ];

    let suspicious: Vec<(&String, &CtorStats)> = by_count
        .iter()
        .filter(|(name, _)| SUSPICIOUS_KWS.iter().any(|kw| name.contains(kw)))
        .map(|(n, s)| (*n, *s))
        .collect();

    // ── Retaining paths via reverse edge graph ───────────────────────────────
    // Skip for very large snapshots to keep RAM sane.
    let skip_retaining = node_count > 5_000_000;
    let mut retaining_paths: Vec<String> = Vec::new();

    if !skip_retaining {
        eprintln!("Building reverse edge map for retaining paths…");
        // One parent per node (first strong parent encountered)
        let mut parent = vec![-1i32; node_count];

        for i in 0..node_count {
            let start = edge_start[i] as usize;
            let end = edge_start[i + 1] as usize;
            for e in start..end {
                let e_base = e * efc;
                if e_base + ef_to >= edges.len() {
                    break; // guard against malformed snapshot
                }
                let edge_type_idx = edges[e_base + ef_type] as usize;
                let to_offset = edges[e_base + ef_to] as usize;
                let to_idx = to_offset / nfc;
                if to_idx < node_count && parent[to_idx] == -1 {
                    let et = edge_types.get(edge_type_idx).copied().unwrap_or("");
                    if et != "weak" {
                        parent[to_idx] = i as i32;
                    }
                }
            }
        }

        let node_label = |i: usize| -> String {
            let base = i * nfc;
            let type_idx = nodes[base + nf_type] as usize;
            let name_idx = nodes[base + nf_name] as usize;
            let type_name = node_types.get(type_idx).copied().unwrap_or("?");
            let name = strings.get(name_idx).map(|s| s.as_str()).unwrap_or("");
            if name.is_empty() {
                format!("({type_name})")
            } else {
                format!("{type_name}:{name}")
            }
        };

        let nf_det = match nf_detached {
            Some(v) => v,
            None => {
                eprintln!("No detachedness field — skipping retaining paths");
                usize::MAX
            }
        };

        if nf_det != usize::MAX {
            let mut sampled = 0usize;
            for i in 0..node_count {
                if sampled >= 8 {
                    break;
                }
                let base = i * nfc;
                if nodes[base + nf_det] != 2 {
                    continue;
                }
                let type_idx = nodes[base + nf_type] as usize;
                let type_name = node_types.get(type_idx).copied().unwrap_or("");
                if type_name != "object" && type_name != "native" {
                    continue;
                }

                let mut path: Vec<String> = Vec::new();
                let mut cur = i as i32;
                let mut seen = std::collections::HashSet::new();
                while cur >= 0 && !seen.contains(&cur) && path.len() < 12 {
                    seen.insert(cur);
                    path.push(node_label(cur as usize));
                    if cur == 0 {
                        break;
                    }
                    cur = parent[cur as usize];
                }
                if cur > 0 {
                    path.push("…(root not reached)".to_string());
                }
                retaining_paths.push(path.join(" ← "));
                sampled += 1;
            }
        }
    }

    // ── Build report ─────────────────────────────────────────────────────────
    let mut out = String::with_capacity(64 * 1024);

    let fname = PathBuf::from(file_path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    macro_rules! ln {
        ($($arg:tt)*) => {
            out.push_str(&format!($($arg)*));
            out.push('\n');
        };
    }

    ln!("# Heap Snapshot Analysis");
    ln!("");
    ln!("**File:** {} ({:.1} MB)", fname, file_size as f64 / 1_048_576.0);
    ln!("**Unix timestamp:** {}", unix_ts());
    ln!("**Nodes:** {}", node_count);
    ln!("**Edges:** {}", edge_count);
    ln!("**Total shallow size:** {}", fmt_bytes(total_size));
    ln!("");

    // Top 50 by shallow size
    ln!("## Top 50 Constructors by Shallow Size");
    ln!("");
    ln!("```");
    ln!("{:<52} {:>8}  {:>12}  {:>7}", "Constructor", "Count", "ShallowSize", "%Total");
    ln!("{}", "-".repeat(86));
    for (name, v) in by_size.iter().take(50) {
        ln!(
            "{:<52} {:>8}  {:>12}  {:>7}",
            truncate(name, 51),
            v.count,
            fmt_bytes(v.shallow),
            pct(v.shallow, total_size)
        );
    }
    ln!("```");
    ln!("");

    // Top 30 by count
    ln!("## Top 30 Constructors by Instance Count");
    ln!("");
    ln!("```");
    ln!("{:<52} {:>8}  {:>12}", "Constructor", "Count", "ShallowSize");
    ln!("{}", "-".repeat(75));
    for (name, v) in by_count.iter().take(30) {
        ln!(
            "{:<52} {:>8}  {:>12}",
            truncate(name, 51),
            v.count,
            fmt_bytes(v.shallow)
        );
    }
    ln!("```");
    ln!("");

    // Detached DOM
    ln!("## Detached DOM / Native Nodes");
    ln!("");
    ln!(
        "**Total detached shallow size:** {} ({} of heap)",
        fmt_bytes(total_det_size),
        pct(total_det_size, total_size)
    );
    ln!("");
    if det_sorted.is_empty() {
        ln!("_No detached nodes found (or detachedness field absent in this snapshot version)._");
    } else {
        ln!("```");
        ln!("{:<46} {:>8}  {:>12}", "Detached type", "Count", "ShallowSize");
        ln!("{}", "-".repeat(70));
        for (name, (count, size)) in det_sorted.iter().take(30) {
            ln!(
                "{:<46} {:>8}  {:>12}",
                truncate(name, 45),
                count,
                fmt_bytes(*size)
            );
        }
        ln!("```");
    }
    ln!("");

    // Framework / suspicious
    ln!("## Framework / Suspicious Constructors");
    ln!("");
    if suspicious.is_empty() {
        ln!("_No framework-specific constructors detected._");
    } else {
        ln!("```");
        ln!(
            "{:<52} {:>8}  {:>12}  {:>14}  {:>12}",
            "Constructor", "Count", "ShallowSize", "DetachedCount", "DetachedSize"
        );
        ln!("{}", "-".repeat(105));
        for (name, v) in suspicious.iter().take(30) {
            ln!(
                "{:<52} {:>8}  {:>12}  {:>14}  {:>12}",
                truncate(name, 51),
                v.count,
                fmt_bytes(v.shallow),
                v.det_count,
                fmt_bytes(v.det_size)
            );
        }
        ln!("```");
    }
    ln!("");

    // Retaining paths
    ln!("## Sample Retaining Paths (Detached Nodes → GC Root)");
    ln!("");
    if skip_retaining {
        ln!(
            "_Skipped: {node_count} nodes exceeds 5M limit for reverse-graph build._"
        );
        ln!("_Use Chrome DevTools Retainers panel for top suspects above._");
    } else if retaining_paths.is_empty() {
        ln!("_No detached object/native nodes sampled._");
    } else {
        for (i, path) in retaining_paths.iter().enumerate() {
            ln!("{}. `{}`", i + 1, path);
        }
    }
    ln!("");

    // Pattern hints
    ln!("## Auto-detected Pattern Hints");
    ln!("");

    let ctor_names_lower: Vec<String> = ctor_map.keys().map(|s| s.to_lowercase()).collect();
    let has_vnodes = ctor_names_lower
        .iter()
        .any(|n| n.contains("vnode") || n.contains("component"));
    let has_listeners = ctor_names_lower
        .iter()
        .any(|n| n.contains("listener") || n.contains("emitter") || n.contains("subscription"));
    let has_large_det = total_det_size > 10 * 1_048_576;
    let arr_shallow = ctor_map
        .get("(array)")
        .map(|v| v.shallow)
        .unwrap_or(0);
    let has_big_arrays = arr_shallow > 50 * 1_048_576;
    let has_proxy = ctor_names_lower.iter().any(|n| n.contains("proxy") || n.contains("reactive"));
    let has_stores = ctor_names_lower.iter().any(|n| n.contains("store") || n.contains("cache") || n.contains("registry"));

    if has_vnodes {
        ln!("- **Vue VNode/Component objects prominent** — component instances likely not released after unmount/route change. Audit composables for missing `onUnmounted` cleanup; check global stores retaining VNode or component refs.");
    }
    if has_listeners {
        ln!("- **EventListener/Subscription objects present** — audit `addEventListener`, event bus `.on()`, Pinia/Vuex `$subscribe`, RxJS subscriptions, mitt emitters. All must be removed in `onUnmounted`.");
    }
    if has_large_det {
        ln!(
            "- **Detached DOM: {}** — DOM removed from tree but JS still holds references. Common causes: tooltip/modal/portal/chart/editor instances not destroyed, `v-if` removing without cleanup.",
            fmt_bytes(total_det_size)
        );
    }
    if has_big_arrays {
        ln!(
            "- **Large (array) shallow size: {}** — investigate growing caches, history stacks, event listener registries, render queues. Look for unbounded push without eviction.",
            fmt_bytes(arr_shallow)
        );
    }
    if has_proxy {
        ln!("- **Reactive/Proxy objects present** — Vue 3 reactive objects in memory. Check if reactive state persists beyond component lifecycle (e.g. module-level `reactive()`, composables created outside setup).");
    }
    if has_stores {
        ln!("- **Store/Cache/Registry objects present** — verify stores flush stale entries. Pinia stores with `$patch` history or cached query results can grow unbounded.");
    }
    ln!("- **Next step:** Take a baseline snapshot, repeat the leaking action 5×, force GC, take second snapshot. Use DevTools **Comparison** view sorted by retained size delta.");
    ln!("- **In DevTools:** Expand top suspicious constructor → select an instance → inspect **Retainers** panel to find exact holding reference chain.");
    ln!("");

    out
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    // Walk back to a char boundary so we don't split a multi-byte codepoint
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── Entry point ──────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: heap-analyzer <file.heapsnapshot>");
        eprintln!("");
        eprintln!("  Parses a Chrome .heapsnapshot and writes <file>-analysis.md");
        eprintln!("  suitable for pasting into an LLM for memory leak diagnosis.");
        eprintln!("");
        eprintln!("  Tip: build with 'cargo build --release' for best performance.");
        std::process::exit(1);
    }

    let file_path = &args[1];
    let file_size = std::fs::metadata(file_path)
        .map(|m| m.len())
        .unwrap_or(0);

    eprintln!(
        "Loading {} ({:.1} MB)…",
        file_path,
        file_size as f64 / 1_048_576.0
    );

    let file = File::open(file_path).unwrap_or_else(|e| {
        eprintln!("Error opening file: {e}");
        std::process::exit(1);
    });

    // 8 MB read buffer helps throughput on large files
    let reader = BufReader::with_capacity(8 * 1024 * 1024, file);

    eprintln!("Parsing JSON (streaming — this may take a minute for large files)…");
    let snap: HeapSnapshot = serde_json::from_reader(reader).unwrap_or_else(|e| {
        eprintln!("JSON parse error: {e}");
        std::process::exit(1);
    });

    let report = analyze(&snap, file_path, file_size);

    let out_path = file_path.replace(".heapsnapshot", "") + "-analysis.md";
    let out_file = File::create(&out_path).unwrap_or_else(|e| {
        eprintln!("Failed to create output file: {e}");
        std::process::exit(1);
    });
    let mut writer = BufWriter::new(out_file);
    writer.write_all(report.as_bytes()).unwrap_or_else(|e| {
        eprintln!("Failed to write report: {e}");
        std::process::exit(1);
    });

    // Quick summary to stdout
    let total_size_approx: u64 = report
        .lines()
        .find(|l| l.contains("Total shallow size"))
        .and_then(|l| l.split("**").nth(2))
        .map(|_| 0)
        .unwrap_or(0);
    let _ = total_size_approx;

    eprintln!("");
    eprintln!("Report saved → {out_path}");
    eprintln!("Paste the .md into your LLM for diagnosis.");
}
