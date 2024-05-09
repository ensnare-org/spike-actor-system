use crossbeam_channel::Sender;

#[derive(Debug)]
pub struct Subscription<A: Clone> {
    subscribers: Vec<Sender<A>>,
}
impl<A: Clone> Default for Subscription<A> {
    fn default() -> Self {
        Self {
            subscribers: Default::default(),
        }
    }
}
impl<A: Clone> Subscription<A> {
    pub fn subscribe(&mut self, sender: &Sender<A>) {
        self.subscribers.push(sender.clone());
    }

    pub fn unsubscribe(&mut self, sender: &Sender<A>) {
        self.subscribers.retain(|s| !s.same_channel(sender));
    }

    /// Broadcasts to all subscribers, ignoring errors.
    pub fn broadcast(&self, action: A) {
        for sender in self.subscribers.iter() {
            let r = sender.try_send(action.clone());
            if let Err(e) = r {
                eprintln!("Subscription: while broadcasting: {e:?}");
            }
        }
    }

    /// Broadcasts to all subscribers, removing any that fail to send
    /// successfully.
    pub fn broadcast_mut(&mut self, action: A) {
        self.subscribers
            .retain(|sender| sender.try_send(action.clone()).is_ok());
    }
}
