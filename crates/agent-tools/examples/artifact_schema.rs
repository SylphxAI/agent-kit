//! Print the artifact tool input schema for hosts that register tools externally.

fn main() {
    println!("{}", agent_tools::artifact::artifact_tool_schema());
}
