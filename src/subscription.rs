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

    pub fn subscribers(&self) -> &[Sender<A>] {
        &self.subscribers
    }

    pub fn subscribers_mut(&mut self) -> &mut Vec<Sender<A>> {
        &mut self.subscribers
    }

    pub fn broadcast(&self, action: A) {
        for sender in self.subscribers.iter() {
            let _ = sender.try_send(action.clone());
        }
    }
}
