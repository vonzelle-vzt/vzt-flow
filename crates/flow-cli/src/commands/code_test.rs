//! Hidden diagnostic: runs the deterministic code-mode transform on
//! arbitrary text, no model/microphone involved.

use anyhow::Result;
use flow_core::codemode;

pub fn run(text: &str) -> Result<()> {
    println!("input : {text:?}");
    println!("output: {:?}", codemode::transform(text));
    Ok(())
}
