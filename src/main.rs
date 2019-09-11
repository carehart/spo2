mod health_checker;
mod response;
mod routes;

use std::{env, io, str, thread};

use futures::channel::mpsc::{self, Sender};
use futures::executor::ThreadPool;
use futures::stream::StreamExt;
use url::Url;

use self::health_checker::health_checker;
use self::routes::{update_url, read_url, delete_url, get_all_urls};

const HTTP_LISTEN_ADDR: &str = "HTTP_LISTEN_ADDR";
const WS_LISTEN_ADDR: &str = "WS_LISTEN_ADDR";
const SLACK_HOOK_URL: &str = "SLACK_HOOK_URL";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReportStatus {
    Unhealthy,
    Healthy,
}

pub struct State {
    thread_pool: ThreadPool,
    notifier_sender: Sender<(Url, ReportStatus)>,
    event_sender: ws::Sender,
    database: sled::Db,
}

fn main() -> Result<(), io::Error> {
    let thread_pool = ThreadPool::new().unwrap();
    let (notifier_sender, receiver) = mpsc::channel(100);
    let database = sled::Db::open("spo2.db").unwrap();

    let ws_listen_addr = match env::var(WS_LISTEN_ADDR) {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("{}: {}", WS_LISTEN_ADDR, e);
            String::from("127.0.0.1:8001")
        },
    };

    let http_listen_addr = match env::var(HTTP_LISTEN_ADDR) {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("{}: {}", HTTP_LISTEN_ADDR, e);
            String::from("127.0.0.1:8000")
        },
    };

    // initialize the notifier sender
    thread_pool.spawn_ok(async move {
        let slack_hook_url = match env::var(SLACK_HOOK_URL) {
            Ok(url) => url,
            Err(e) => {
                eprintln!("{}: {}", SLACK_HOOK_URL, e);
                return
            }
        };

        let mut receiver = receiver;
        while let Some((url, status)) = receiver.next().await {
            let body = format!("{} reported {:?}", url, status);
            let body = serde_json::json!({ "text": body });
            let request = surf::post(&slack_hook_url).body_json(&body).unwrap();
            if let Err(e) = request.recv_string().await {
                eprintln!("{}", e);
            }
        }
    });

    // initialize the websocket listener
    let builder = ws::Builder::new();
    let ws = builder.build(|_| |_| Ok(())).unwrap();
    let event_sender = ws.broadcaster();

    // run the websocket listener
    println!("Websocket server is listening on: ws://{}", ws_listen_addr);
    let _ = thread::spawn(|| {
        ws.listen(ws_listen_addr).expect("websocket listen error")
    });

    // run health checking for every url saved
    for result in database.iter() {
        let key = match result {
            Ok((key, _)) => key,
            Err(e) => { eprintln!("{}", e); continue },
        };

        let string = str::from_utf8(&key).unwrap();
        let url = Url::parse(string).unwrap();

        let notifier_sender = notifier_sender.clone();
        let database = database.clone();
        let event_sender = event_sender.clone();

        thread_pool.spawn_ok(async {
            health_checker(url, notifier_sender, event_sender, database).await
        });
    }

    let state = State { thread_pool, notifier_sender, event_sender, database };
    let mut app = tide::App::with_state(state);

    app.at("/:url")
        .post(update_url)
        .get(read_url)
        .put(update_url)
        .delete(delete_url);

    app.at("/")
        .get(get_all_urls);

    // start listening to clients now
    app.run(http_listen_addr)
}
