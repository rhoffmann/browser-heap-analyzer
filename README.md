# heap-analyzer

A fast Rust CLI that converts Chrome `.heapsnapshot` files into concise Markdown reports optimized for LLM-assisted memory leak diagnosis.

## What it does

- Parses the raw V8 heap snapshot format (nodes, edges, strings)
- Aggregates constructors by shallow size and instance count
- Detects detached DOM / native nodes and their retaining paths
- Flags framework-specific objects (Vue, Pinia, EventListeners, Proxies, etc.)
- Emits auto-detected pattern hints with actionable next steps
- Outputs a single `.md` file ready to paste into any LLM

## Usage

```sh
cargo build --release
./target/release/heap-analyzer path/to/snapshot.heapsnapshot
# writes  path/to/snapshot-analysis.md
```

## Output sections

| Section | Description |
|---|---|
| Top 50 by shallow size | Largest allocating constructors |
| Top 30 by count | Most frequently instantiated types |
| Detached DOM | Nodes removed from tree but still referenced |
| Framework constructors | Vue/Pinia/EventEmitter/Proxy suspects |
| Retaining paths | Sample GC root chains for detached objects |
| Pattern hints | Automated diagnosis with fix suggestions |

## Requirements

- Rust 2021 edition or later
- Input: a `.heapsnapshot` file from Chrome DevTools → Memory → "Take snapshot"
