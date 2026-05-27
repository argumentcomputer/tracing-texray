# tracing-texray [![Latest Version]][crates.io]
[Latest Version]: https://img.shields.io/crates/v/tracing-texray.svg
[crates.io]: https://crates.io/crates/tracing-texray

`tracing-texray` is a [tracing](https://tracing.rs) layer to introspect spans and events in plain text. By `examine`-ing a specific
span, a full tree will be output when that span exits. Using code like the following (actual program elided):

```rust
fn main() {
    // initialize & install as the global subscriber
    tracing_texray::init();
    // examine the `load_data` span:
    tracing_texray::examine(tracing::info_span!("load_data")).in_scope(|| {
        do_a_thing()
    });
}

fn do_a_thing() {
    // ...
}
```

You would see the following output printed to stderr:

```text
load_data                                52ms ├────────────────────────────────┤
  download_results{uri: www.crates.io}   11ms                ├─────┤
   >URI resolved                                             ┼
   >connected                                                   ┼
  compute_stats                          10ms                        ├─────┤
  render_response                         6ms                               ├──┤
```

In cases where a more powerful solution like [tracing-chrome](https://crates.io/crates/tracing-chrome) is not required,
`tracing-texray` can render lightweight timeline of what happened when.

## Usage

`tracing-texray` combines two pieces: a global subscriber, and local span examination. By default, `tracing-texray` won't
print anything—it just sits in the background. But: once a span is `examine`'d, `tracing-texray` will track the
span and all of its children. When the span exits, span diagnostics will be printed to stderr (or another `impl io::Write`
as configured).

**First**, the layer must be installed globally:

```rust,no_run
use std::time::Duration;
use tracing_texray::TeXRayLayer;
use tracing_subscriber::{Registry, EnvFilter, layer::SubscriberExt};

// Option A: Exclusively using tracing_texray:
tracing_texray::init();

// Option B: install the layer in combination with other layers, eg. tracing_subscriber::fmt:
let subscriber = Registry::default()
    .with(EnvFilter::try_from_default_env().expect("invalid env filter"))
    .with(tracing_subscriber::fmt::layer())
    .with(
        TeXRayLayer::new()
            // by default, all metadata fields will be printed. If this is too noisy,
            // filter only the fields you care about
            .only_show_fields(&["name", "operation", "service"])
            // only print spans longer than a certain duration
            .min_duration(Duration::from_millis(100)),
    );
tracing::subscriber::set_global_default(subscriber).unwrap();
```

**Next**, wrap any spans you want to track with `examine`:

```rust
use tracing::info_span;
use tracing_texray::examine;

fn somewhere_deep_in_my_program() {
    tracing_texray::examine(info_span!("do_a_thing")).in_scope(|| {
        for id in 0..5 {
            some_other_function(id);
        }
    })
}

fn some_other_function(id: usize) {
    info_span!("inner_task", id = %id).in_scope(|| tracing::info!("buzz"));
    // ...
}
```

For functions decorated with `#[tracing::instrument]`, the span handle isn't
directly available — call `examine_current()` from inside the function body
instead:

```rust
use tracing::info_span;

#[tracing::instrument(name = "do_a_thing", skip_all)]
fn somewhere_deep_in_my_program() {
    tracing_texray::examine_current();
    for id in 0..5 {
        info_span!("inner_task", id = %id).in_scope(|| tracing::info!("buzz"));
    }
}
```

When the `do_a_thing` span exits, output like the following will be printed: 
```text
do_a_thing           509μs ├───────────────────────────────────────────────────┤
  inner_task{id: 0}   92μs         ├────────┤
   >buzz                             ┼
  inner_task{id: 1}   36μs                       ├──┤
   >buzz                                         ┼
  inner_task{id: 2}   35μs                               ├──┤
   >buzz                                                 ┼
  inner_task{id: 3}   36μs                                         ├──┤
   >buzz                                                           ┼
  inner_task{id: 4}   35μs                                                 ├──┤
   >buzz                                                                   ┼
```

## Streaming per-span stats

`TeXRayLayer::streaming()` emits a one-line summary as each tracked span
closes, in addition to the timeline that prints when the `examine`'d root
exits:

```rust
use tracing_texray::TeXRayLayer;

let layer = TeXRayLayer::new().streaming();
# drop(layer);
```

```text
[texray] stage1_commit: 30.12ms
[texray] prove: 1.40s
```

To include RSS delta and peak alongside the duration, call `track_ram()` —
it enables RAM sampling *and* turns on streaming (streaming is the only
output channel for the RAM data, so `track_ram()` implies it):

```rust
use tracing_texray::TeXRayLayer;

let layer = TeXRayLayer::new().track_ram();
# drop(layer);
```

```text
[texray] stage1_commit: 30.12ms  ── RAM Δ +48.00 MiB peak 371.90 MiB
[texray] prove: 1.40s  ── RAM Δ +18.00 MiB peak 471.96 MiB
```

The `Δ` is the net RSS change between entry and exit; `peak` is `VmHWM` at
exit. A span ending well below its peak (e.g. `prove` above with Δ +18 MiB
but peak 472 MiB) freed memory before returning. RSS sampling reads
`/proc/self/status`, so it's Linux-only — non-Linux samples report zero and
the RAM suffix is suppressed.
