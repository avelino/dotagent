// hello-rust — minimal Rust example agent for dotagent.
fn main() {
    let vars = [
        "AGENT_NAME",
        "AGENT_HOME",
        "AGENT_TMPDIR",
        "AGENT_DRY_RUN",
        "AGENT_SCHEDULE_ID",
        "AGENT_START_EPOCH",
        "AGENT_ARGV",
        "AGENT_HEARTBEAT_FILE",
    ];
    println!("=== hello from rust agent ===");
    for k in vars {
        println!("{k:<22} = {}", std::env::var(k).unwrap_or_default());
    }
    println!(
        "std::env::args         = {:?}",
        std::env::args().collect::<Vec<_>>()
    );
}
