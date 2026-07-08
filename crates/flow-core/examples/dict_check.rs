//! Ad-hoc verification helper (not part of the CLI): runs the real
//! dictionary correction pass on an arbitrary string, e.g. a raw
//! `flow transcribe` output, so the fix can be eyeballed end-to-end
//! against the exact seeded dictionary shipped in `dictionary.rs`.
fn main() {
    let input = std::env::args().nth(1).expect("usage: dict_check <text>");
    let dict = flow_core::dictionary::seed_dictionary();
    println!("{}", flow_core::dictionary::correct(&input, &dict));
}
