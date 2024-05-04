use crossbeam_channel::{Receiver, Sender};

pub trait ProvidesService<I, E> {
    fn receiver(&self) -> &Receiver<E>;
    fn sender(&self) -> &Sender<I>;
    fn send_input(&self, input: I) {
        let _ = self.sender().try_send(input);
    }
}

pub trait ProvidesActorService<R, A> {
    /// Send side of channel for service requests.
    fn sender(&self) -> &Sender<R>;
    /// Convenience method to send requests.
    fn send_request(&self, request: R) {
        let _ = self.sender().try_send(request);
    }
}
