//! `pots-mock`: run the fake LLM server for trying Pots without an API key.

use std::time::Duration;

use pots_mock::{MockServer, Script};

const USAGE: &str = "\
usage: pots-mock [--port PORT] [--delay-ms MS]

  --port PORT     port to listen on, on 127.0.0.1 (default 4000; 0 picks a free one)
  --delay-ms MS   pause between streamed pieces of a reply (default 30)";

fn main() {
    let (mut port, mut delay_ms) = (4000u16, 30u64);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or_else(|| fail(&format!("{name} needs a number")))
        };
        match arg.as_str() {
            "--port" => port = u16::try_from(value("--port")).unwrap_or_else(|_| fail("--port is too big")),
            "--delay-ms" => delay_ms = value("--delay-ms"),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown argument {other:?}")),
        }
    }

    let server = MockServer::bind(("127.0.0.1", port), Script::new(), Duration::from_millis(delay_ms))
        .unwrap_or_else(|e| fail(&format!("can't listen on 127.0.0.1:{port}: {e}")));
    let url = server.url();
    println!(
        "pots-mock listening on {url}

Point an agent at it in ~/.pots/config.toml:

[providers.mock]
api = \"anthropic\"            # or \"openai\" with base_url = \"{url}/v1\"
base_url = \"{url}\"

[[agents]]
name = \"Mock\"
provider = \"mock\"
model = \"mock\"

In chat, `!tool NAME {{json}}` makes it call a tool and `!error 529 msg` fails a request.
Ctrl-C to stop."
    );
    loop {
        std::thread::park();
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("pots-mock: {msg}\n\n{USAGE}");
    std::process::exit(2);
}
