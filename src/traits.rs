use crossbeam_channel::{Receiver, Sender};

pub trait ProvidesService<I, E> {
    fn receiver(&self) -> &Receiver<E>;
    fn sender(&self) -> &Sender<I>;
    fn send_input(&self, input: I) {
        let _ = self.sender().try_send(input);
    }

    fn recv_operation<T>(
        oper: crossbeam_channel::SelectedOperation,
        r: &Receiver<T>,
    ) -> Result<T, crossbeam_channel::RecvError> {
        let input_result = oper.recv(r);
        if let Err(e) = input_result {
            eprintln!(
                "ProvidesService: While attempting to receive from {:?}: {}",
                *r, e
            );
        }
        input_result
    }
}

pub trait ProvidesActorService<R, A> {
    /// Send side of channel for service requests.
    fn sender(&self) -> &Sender<R>;
    /// Convenience method to send requests.
    fn send_request(&self, request: R) {
        let _ = self.sender().try_send(request);
    }
    /// Send side of action channel, which allows subscriptions to other
    /// entities' actions.
    fn action_sender(&self) -> &Sender<A>;

    fn recv_operation<T>(
        oper: crossbeam_channel::SelectedOperation,
        r: &Receiver<T>,
    ) -> Result<T, crossbeam_channel::RecvError> {
        let input_result = oper.recv(r);
        if let Err(e) = input_result {
            eprintln!(
                "ProvidesActorService: While attempting to receive from {:?}: {}",
                *r, e
            );
        }
        input_result
    }
}
