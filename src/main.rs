mod app;
mod chooser;
mod font;
mod fs;
mod mime;
mod modal;
mod ops;
mod player;
mod render;
mod theme;
mod thumbs;
mod video;
mod view;
mod watch;
mod xkb;

use std::{env, path::PathBuf};

use smithay_client_toolkit::reexports::{
    calloop::{EventLoop, channel},
    client::Connection,
};

fn main() {
    // A file dialog for the portal, or the file manager in the home folder.
    let args: Vec<_> = env::args_os().skip(1).collect();
    let (chooser, start, select) = match chooser::from_args(&args) {
        Some((chooser, start, select)) => (Some(chooser), start, select),
        None => {
            let home = env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
            (None, home, None)
        }
    };
    // Start reading it while we handshake with the compositor.
    let (loader, loads) = channel::channel();
    app::spawn_load(loader.clone(), 1, start.clone(), false);

    let conn = Connection::connect_to_env().expect("no Wayland compositor");
    let mut event_loop = EventLoop::try_new().expect("event loop");
    let mut app = app::App::new(
        &conn,
        event_loop.handle(),
        loader,
        loads,
        start,
        chooser,
        select,
    );

    let signal = event_loop.get_signal();
    event_loop
        .run(None, &mut app, |app| {
            if app.exit {
                signal.stop();
            }
        })
        .expect("event loop");
}
