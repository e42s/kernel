use std::io;
use mio::{Poll, Token, Ready, Events, Evented, PollOpt};

pub trait EventHandler {
    fn handle(&mut self, el: &mut EventLoop, token: Token, events: Ready);
}

pub struct EventLoop {
    events_poller: Poll,
    events: Events,
    running: bool
}

impl EventLoop {
    pub fn new() -> io::Result<EventLoop> {
        let evts = Events::with_capacity(1024);
        let poll = try!(Poll::new());
        let event_loop = EventLoop {
            events_poller: poll,
            events: evts,
            running: false
        };

        Ok(event_loop)
    }

    pub fn shutdown(&mut self) {
        self.running = false;
    }

    pub fn run<H: EventHandler>(&mut self, event_handler: &mut H) -> io::Result<()> {
        self.running = true;

        while self.running {
            try!(self.run_once(event_handler));
        }

        Ok(())
    }

    pub fn run_once<H: EventHandler>(&mut self, event_handler: &mut H) -> io::Result<()> {
        let event_count = match self.poll_events() {
            Ok(count) => count,
            Err(err) => {
                if err.kind() == io::ErrorKind::Interrupted {
                    0
                } else {
                    return Err(err);
                }
            }
        };

        self.process_events(event_handler, event_count);

        Ok(())
    }

    fn poll_events(&mut self) -> io::Result<usize> {
        self.events_poller.poll(&mut self.events, None)
    }

    fn process_events<H: EventHandler>(&mut self, event_handler: &mut H, count: usize) {
        let mut i = 0;

        while i < count {
            let event = self.events.get(i).unwrap();

            event_handler.handle(self, event.token(), event.kind());

            i += 1;
        }
    }

    pub fn register(&mut self, io: &Evented, token: Token, interest: Ready, opt: PollOpt) -> io::Result<()> {
        self.events_poller.register(io, token, interest, opt)
    }
    pub fn reregister(&mut self, io: &Evented, token: Token, interest: Ready, opt: PollOpt) -> io::Result<()> {
        self.events_poller.reregister(io, token, interest, opt)
    }
    pub fn deregister(&mut self, io: &Evented) -> io::Result<()> {
        self.events_poller.deregister(io)
    }
}
