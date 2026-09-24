/// Init the tracing subscriber based on -v/-q.
pub fn init(verbose: u8, quiet: bool) {
    use tracing_subscriber::{EnvFilter, fmt};

    let level = if quiet {
        "error"
    } else {
        match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        }
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let _ = fmt().with_env_filter(filter).try_init();
}
