//! Hidden diagnostic command: exercises the clipboard save/set/[Cmd+V]/
//! restore pipeline in isolation, honestly reporting permission state
//! instead of pretending a real paste happened when it couldn't.

pub fn run(text: &str) -> anyhow::Result<()> {
    flow_core::insert::run_paste_test(text)
}
