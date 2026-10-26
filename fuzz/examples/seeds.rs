fn main() -> std::io::Result<()> {
    let root = std::env::args_os()
        .nth(1)
        .expect("usage: seeds <corpus-directory>");
    kasu_fuzz::seeds::generate(std::path::Path::new(&root))
}
