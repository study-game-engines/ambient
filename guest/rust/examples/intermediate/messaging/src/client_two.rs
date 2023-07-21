use ambient_api::prelude::*;

#[main]
pub fn main() {
    // This module will receive messages from `client.rs`, and respond to them.
    messages::this::Local::subscribe(move |source, data| {
        println!("{source:?}: {data:?}");
        if let Some(id) = source.local() {
            messages::this::Local::new("Hi, back!").send_local(id);
        }
    });
}
