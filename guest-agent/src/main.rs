#[path = "../../src/protocol.rs"]
mod protocol;

#[path = "../../src/guest_agent.rs"]
mod guest_agent;

fn main() {
    guest_agent::main();
}
