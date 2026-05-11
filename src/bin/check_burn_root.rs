fn main() {
    let root = miden_standards::note::BurnNote::script_root();
    println!(
        "BURN root from THIS PROJECT's deps: 0x{}",
        hex::encode(root.as_bytes())
    );
}
