use rakrs::Motd;
use rakrs::RakEvent;
use rakrs::RakNetServer;
use rakrs::RakResult;
use rakrs::start;
use futures::Future;
use tokio::runtime::Handle;
use tokio::runtime::Runtime;


#[test]
pub fn test_boot() {
    let server = RakNetServer::new(String::from("0.0.0.0:19132"));
    let motd = Motd {
        name: "Netrex Raknet".to_owned(),
        protocol: 190,
        player_count: 0,
        player_max: 10000,
        gamemode: "creative".to_owned(),
        version: "1.18.9".to_owned(),
        server_id: 2747994720109207713 as i64,
    };
    let channel = netrex_events::Channel::<RakEvent, RakResult>::new();
    let mut unknown = 0;
    let mut listener = |event, result| {
        match event {
            RakEvent::ConnectionCreated(_) => {
                println!("Client connected");
            }
            RakEvent::Disconnect(_, _) => {
                println!("Client disconnected");
            }
            _ => {
                unknown += 1;
                println!("Unknown events: {}", unknown);
            }
        };
        None
    };
    channel.receive(&mut listener);

    // this is a hack!
    // get the tokio runtime and wait pulling!
    println!("Hi I am running concurrently.");
    let rt = Runtime::new().unwrap();
    let handle = rt.handle();
    handle.block_on(async move {
        start(server, channel).await;
    });
}