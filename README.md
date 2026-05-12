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

## RAM tracking

`TeXRayLayer::track_ram()` enables per-span RSS sampling. Each examined span
records the process's resident-set size on entry and exit, plus the
high-water mark (`VmHWM`). Below the timeline, a `RAM:` block shows the
trajectory and peak per span:

```rust
use tracing_texray::TeXRayLayer;

let layer = TeXRayLayer::new().track_ram();
# drop(layer);
```

```text
prove                          1.4s ├──────────────────────────────────────┤
  stage1_commit                30ms ├─┤
  ...

RAM:
  prove           RSS 320.00 MiB → 338.00 MiB (Δ +18.00 MiB)   peak 471.96 MiB
    stage1_commit RSS 320.00 MiB → 368.00 MiB (Δ +48.00 MiB)   peak 371.90 MiB
    ...
```

The `RSS start → end` trajectory plus the `peak` make transient allocations
visible: a span ending well below its peak (e.g. `prove` above ending at
338 MiB but peaking at 472) freed memory before exiting. RSS sampling reads
`/proc/self/status`, so it's Linux-only — non-Linux samples report zero and
the `RAM:` block is suppressed.
