use crossbeam_channel::{Receiver, Sender};

pub trait ProvidesService<I, E> {
    fn receiver(&self) -> &Receiver<E>;
    fn sender(&self) -> &Sender<I>;
}

pub trait ProvidesActorService<I, E> {
    fn receiver(&self) -> &Receiver<E>;
    fn sender(&self) -> &Sender<I>;
}
