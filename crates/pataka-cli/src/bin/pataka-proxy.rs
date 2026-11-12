//! `pataka-proxy`: the same program as `pataka`, under the name the install
//! scripts use.

fn main() {
    std::process::exit(pataka_cli::run())
}
