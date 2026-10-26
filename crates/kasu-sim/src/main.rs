fn main() {
    let mut seed = 42;
    let mut clients = 1000;
    let mut scenario = String::from("land");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scenario" => scenario = args.next().expect("scenario name"),
            "--seed" => {
                seed = args
                    .next()
                    .expect("seed value")
                    .parse()
                    .expect("integer seed")
            }
            "--clients" => {
                clients = args
                    .next()
                    .expect("client count")
                    .parse()
                    .expect("integer client count")
            }
            _ => {
                eprintln!(
                    "usage: kasu-sim [--scenario land|cache] [--seed <u64>] [--clients <2..10000>]"
                );
                std::process::exit(2);
            }
        }
    }
    let output = match scenario.as_str() {
        "land" => serde_json::to_string_pretty(&kasu_sim::simulate_land(seed, clients)),
        "cache" => serde_json::to_string_pretty(&kasu_sim::simulate_cache(seed, clients)),
        _ => {
            eprintln!("scenario must be land or cache");
            std::process::exit(2);
        }
    };
    println!("{}", output.unwrap());
}
