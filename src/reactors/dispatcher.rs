
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::io::{self, Result, Error, ErrorKind};
use std::fmt;
use std::time::Duration;

use mio::{Token, Ready, PollOpt};
use mio::timer::{Timer, Builder};
use mio::channel::Receiver;

use network::endpoint::{SocketId, DeviceId, EndpointId, EndpointSpec};
use network::transport::Transport;
use network::tcp::{pipe, acceptor};
use network::tcp::pipe::Event;
use reactors::api::{Signal, Task};
use reactors::event_loop::{EventLoop, EventHandler};
use reactors::bus::EventLoopBus;
use network::session;
use network::socket;
use network::endpoint;
use network::device;
use reactors::api;
use reactors::adapter::{Schedule, EndpointCollection, Network, SocketEventLoopContext};
use reactors::sequence::Sequence;

const CHANNEL_TOKEN: Token = Token(::std::usize::MAX - 1);
const BUS_TOKEN: Token = Token(::std::usize::MAX - 2);
const TIMER_TOKEN: Token = Token(::std::usize::MAX - 3);

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Scheduled(usize);

impl From<usize> for Scheduled {
    fn from(value: usize) -> Scheduled {
        Scheduled(value)
    }
}

impl Into<usize> for Scheduled {
    fn into(self) -> usize {
        self.0
    }
}

impl<'x> Into<usize> for &'x Scheduled {
    fn into(self) -> usize {
        self.0
    }
}

pub trait Context: Network + Scheduler + fmt::Debug {
    fn raise(&mut self, evt: Event);
}

pub trait Scheduler {
    fn schedule(&mut self, schedulable: Schedulable, delay: Duration) -> Result<Scheduled>;
    fn cancel(&mut self, scheduled: Scheduled);
}

pub enum Schedulable {
    Reconnect(EndpointId, EndpointSpec),
    Rebind(EndpointId, EndpointSpec),
    SendTimeout,
    RecvTimeout,
    ReqResend,
    SurveyCancel,
}
pub struct Dispatcher {
    channel: Receiver<api::Request>,
    bus: EventLoopBus<Signal>,
    timer: Timer<Task>,
    sockets: session::Session,
    endpoints: EndpointCollection,
    schedule: Schedule,
}

impl Dispatcher {
    pub fn dispatch(transports: HashMap<String, Box<Transport + Send>>,
                    rx: Receiver<api::Request>,
                    tx: Sender<session::Reply>)
                    -> io::Result<()> {

        let mut dispatcher = Dispatcher::new(transports, rx, tx);

        dispatcher.run()
    }
    pub fn new(transports: HashMap<String, Box<Transport + Send>>,
               rx: Receiver<api::Request>,
               tx: Sender<session::Reply>)
               -> Dispatcher {

        let id_seq = Sequence::new();
        let timeout_eq = Sequence::new();
        let clock = Builder::default()
            .tick_duration(Duration::from_millis(25))
            .num_slots(1_024)
            .capacity(8_192)
            .build();

        Dispatcher {
            channel: rx,
            bus: EventLoopBus::new(),
            timer: clock,
            sockets: session::Session::new(id_seq.clone(), tx),
            endpoints: EndpointCollection::new(id_seq.clone(), transports),
            schedule: Schedule::new(timeout_eq),
        }

    }

    pub fn run(&mut self) -> io::Result<()> {
        let mut event_loop = try!(EventLoop::new());
        let interest = Ready::readable();
        let opt = PollOpt::edge();

        try!(event_loop.register(&self.channel, CHANNEL_TOKEN, interest, opt));
        try!(event_loop.register(&self.bus, BUS_TOKEN, interest, opt));
        try!(event_loop.register(&self.timer, TIMER_TOKEN, interest, opt));

        event_loop.run(self)
    }

    fn process_channel(&mut self, el: &mut EventLoop) {
        while let Ok(req) = self.channel.try_recv() {
            self.process_request(el, req);
        }
    }
    fn process_bus(&mut self, el: &mut EventLoop) {
        while let Some(signal) = self.bus.recv() {
            self.process_signal(el, signal);
        }
    }
    fn process_timer(&mut self, el: &mut EventLoop) {
        while let Some(timeout) = self.timer.poll() {
            self.process_tick(el, timeout);
        }
    }

    fn process_request(&mut self, el: &mut EventLoop, request: api::Request) {
        match request {
            api::Request::Session(req) => self.process_session_request(el, req),
            api::Request::Socket(id, req) => self.process_socket_request(el, id, req),
            api::Request::Endpoint(sid, eid, req) => {
                self.process_endpoint_request(el, sid, eid, req)
            }
            api::Request::Device(id, req) => self.process_device_request(el, id, req),
            _ => {}
        }
    }
    fn process_signal(&mut self, el: &mut EventLoop, signal: Signal) {
        match signal {
            Signal::PipeCmd(_, eid, cmd) => self.process_pipe_cmd(el, eid, cmd),
            Signal::AcceptorCmd(_, eid, cmd) => self.process_acceptor_cmd(el, eid, cmd),
            Signal::PipeEvt(sid, eid, evt) => self.process_pipe_evt(el, sid, eid, evt),
            Signal::AcceptorEvt(sid, eid, evt) => self.process_acceptor_evt(el, sid, eid, evt),
            Signal::SocketEvt(sid, evt) => self.process_socket_evt(el, sid, evt),
        }
    }

    fn process_tick(&mut self, _: &mut EventLoop, task: Task) {
        match task {
            Task::Socket(sid, schedulable) => self.process_socket_task(sid, schedulable),
        }
    }

    fn process_socket_task(&mut self, sid: SocketId, task: Schedulable) {
        match task {
            Schedulable::Reconnect(eid, spec) => {
                self.apply_on_socket(sid, |socket, ctx| socket.reconnect(ctx, eid, spec))
            }
            Schedulable::Rebind(eid, spec) => {
                self.apply_on_socket(sid, |socket, ctx| socket.rebind(ctx, eid, spec))
            }
            Schedulable::SendTimeout => {
                self.apply_on_socket(sid, |socket, ctx| socket.on_send_timeout(ctx))
            }
            Schedulable::RecvTimeout => {
                self.apply_on_socket(sid, |socket, ctx| socket.on_recv_timeout(ctx))
            }
            other => self.apply_on_socket(sid, |socket, ctx| socket.on_timer_tick(ctx, other)),
        }
    }

    fn process_io(&mut self, el: &mut EventLoop, token: Token, events: Ready) {
        println!("process_io {:?} {:?}", token, events);
        let eid = EndpointId::from(token);
        {
            if let Some(pipe) = self.endpoints.get_pipe_mut(eid) {
                pipe.ready(el, &mut self.bus, events);
                return;
            }
        }
        {
            if let Some(acceptor) = self.endpoints.get_acceptor_mut(eid) {
                acceptor.ready(el, &mut self.bus, events);
                return;
            }
        }
    }

    fn process_session_request(&mut self, el: &mut EventLoop, request: session::Request) {
        match request {
            session::Request::CreateSocket(ctor) => self.sockets.add_socket(ctor),
            session::Request::CreateDevice(l, r) => {
                self.apply_on_socket(l, |socket, ctx| socket.on_device_plugged(ctx));
                self.apply_on_socket(r, |socket, ctx| socket.on_device_plugged(ctx));
                self.sockets.add_device(l, r);
            }
            session::Request::Shutdown => el.shutdown(),
        }
    }
    fn process_socket_request(&mut self,
                              _: &mut EventLoop,
                              id: SocketId,
                              request: socket::Request) {
        match request {
            socket::Request::Connect(url) => {
                self.apply_on_socket(id, |socket, ctx| socket.connect(ctx, url))
            }
            socket::Request::Bind(url) => {
                self.apply_on_socket(id, |socket, ctx| socket.bind(ctx, url))
            }
            socket::Request::Send(msg) => {
                self.apply_on_socket(id, |socket, ctx| socket.send(ctx, msg))
            }
            socket::Request::Recv => self.apply_on_socket(id, |socket, ctx| socket.recv(ctx)),
            socket::Request::Close => self.apply_on_socket(id, |socket, ctx| socket.close(ctx)),
        }
    }
    fn process_endpoint_request(&mut self,
                                _: &mut EventLoop,
                                sid: SocketId,
                                eid: EndpointId,
                                request: endpoint::Request) {
        let endpoint::Request::Close(remote) = request;

        self.apply_on_socket(sid, |socket, ctx| if remote {
            socket.close_pipe(ctx, eid)
        } else {
            socket.close_acceptor(ctx, eid)
        });
    }
    fn process_device_request(&mut self,
                              _: &mut EventLoop,
                              id: DeviceId,
                              request: device::Request) {
        if let device::Request::Check = request {
            self.apply_on_device(id, |device| device.check())
        }
    }

    fn process_pipe_cmd(&mut self, el: &mut EventLoop, eid: EndpointId, cmd: pipe::Command) {
        if let Some(pipe) = self.endpoints.get_pipe_mut(eid) {
            pipe.process(el, &mut self.bus, cmd);
        }
    }
    fn process_acceptor_cmd(&mut self, el: &mut EventLoop, eid: EndpointId, cmd: pipe::Command) {
        if let Some(acceptor) = self.endpoints.get_acceptor_mut(eid) {
            acceptor.process(el, &mut self.bus, cmd);
        }
    }
    fn process_pipe_evt(&mut self,
                        _: &mut EventLoop,
                        sid: SocketId,
                        eid: EndpointId,
                        evt: pipe::Event) {
        match evt {
            pipe::Event::Opened => {
                self.apply_on_socket(sid, |socket, ctx| socket.on_pipe_opened(ctx, eid))
            }
            pipe::Event::CanSend => {
                self.apply_on_socket(sid, |socket, ctx| socket.on_send_ready(ctx, eid))
            }
            pipe::Event::Sent => {
                self.apply_on_socket(sid, |socket, ctx| socket.on_send_ack(ctx, eid))
            }
            pipe::Event::CanRecv => {
                self.apply_on_socket(sid, |socket, ctx| socket.on_recv_ready(ctx, eid))
            }
            pipe::Event::Accepted(_) => {}
            pipe::Event::Received(msg) => {
                self.apply_on_socket(sid, |socket, ctx| socket.on_recv_ack(ctx, eid, msg))
            }
            pipe::Event::Error(err) => {
                self.apply_on_socket(sid, |socket, ctx| socket.on_pipe_error(ctx, eid, err))
            }
            pipe::Event::Closed => self.endpoints.remove_pipe(eid),
        }
    }
    fn process_acceptor_evt(&mut self,
                            _: &mut EventLoop,
                            sid: SocketId,
                            aid: EndpointId,
                            evt: pipe::Event) {
        match evt {
            // Maybe the controller should be removed from the endpoint collection
            pipe::Event::Error(e) => {
                self.apply_on_socket(sid, |socket, ctx| socket.on_acceptor_error(ctx, aid, e))
            }
            pipe::Event::Accepted(pipes) => {
                for pipe in pipes {
                    let pipe_id = self.endpoints.insert_pipe(sid, pipe);

                    self.apply_on_socket(sid,
                                         |socket, ctx| socket.on_pipe_accepted(ctx, aid, pipe_id));
                }
            }
            _ => {}
        }
    }

    fn process_socket_evt(&mut self, _: &mut EventLoop, sid: SocketId, evt: pipe::Event) {
        match evt {
            pipe::Event::Opened => {}
            pipe::Event::Sent => {}
            pipe::Event::Received(_) => {}
            pipe::Event::Accepted(_) => {}
            pipe::Event::Error(_) => {}
            pipe::Event::CanRecv => {
                self.apply_on_device_link(sid, |device| device.on_socket_can_recv(sid))
            }
            pipe::Event::CanSend => {}
            pipe::Event::Closed => self.sockets.remove_socket(sid),
        }
    }

    fn apply_on_socket<F>(&mut self, id: SocketId, f: F)
        where F: FnOnce(&mut socket::Socket, &mut SocketEventLoopContext)
    {
        if let Some(socket) = self.sockets.get_socket_mut(id) {
            let mut ctx = SocketEventLoopContext::new(id,
                                                      &mut self.bus,
                                                      &mut self.endpoints,
                                                      &mut self.schedule,
                                                      &mut self.timer);

            f(socket, &mut ctx);
        }
    }

    fn apply_on_device<F>(&mut self, id: DeviceId, f: F)
        where F: FnOnce(&mut device::Device)
    {
        if let Some(device) = self.sockets.get_device_mut(id) {
            f(device);
        }
    }

    fn apply_on_device_link<F>(&mut self, id: SocketId, f: F)
        where F: FnOnce(&mut device::Device)
    {
        if let Some(device) = self.sockets.find_device_mut(id) {
            f(device);
        }
    }
}

impl EventHandler for Dispatcher {
    fn handle(&mut self, el: &mut EventLoop, token: Token, events: Ready) {
        if token == CHANNEL_TOKEN {
            return self.process_channel(el);
        }
        if token == BUS_TOKEN {
            return self.process_bus(el);
        }
        if token == TIMER_TOKEN {
            return self.process_timer(el);
        }

        self.process_io(el, token, events)
    }
}
