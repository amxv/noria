// A single async executor keeps the headless gateway's idle footprint small.
// HTTP connections, timers and provider subprocesses sleep on the OS reactor.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = noria::app::run(std::env::args().skip(1).collect()).await {
        eprintln!("noria: {error:#}");
        std::process::exit(1);
    }
}
