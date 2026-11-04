fn main() {
    if let Err(error) = comi_scraper::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
