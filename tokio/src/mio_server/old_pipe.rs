struct PipelinedWriter<S, W>
where S: Stream,
S::Item: Encode<W>,
W: Send + 'static
{
    items: Fuse<S>,
    state: State<<<S::Item as Encode<W>>::Fut as IntoFuture>::Future, W, S::Item>,
}

impl<S, W> PipelinedWriter<S, W>
    where W: Write + Send + 'static,
S: Stream,
S::Item: Encode<W, Error = S::Error>,
S::Error: From<io::Error>
{
    fn new(stream: S, writer: BufWriter<W>) -> PipelinedWriter<S, W> {
        PipelinedWriter {
            items: stream.fuse(),
            state: State::Read(writer.flush()),
        }
    }
}

enum State<Fut, W, Item> {
    Empty,
    Read(Flush<W>),
    Reserve(Reserve<W>, Item),
    Write(Fut),
    Flush(Flush<W>),
}

impl<S, W> Future for PipelinedWriter<S, W>
    where W: Write + Send + 'static,
S: Stream,
S::Item: Encode<W, Error = S::Error>,
S::Error: From<io::Error>
{
    type Item = ();
    type Error = S::Error;

    fn poll(&mut self, mut tokens: &Tokens) -> Poll<(), S::Error> {
        // "steady state" is always Read or Write
        if let State::Flush(_) = self.state {
            panic!("Attempted to poll a PipelinedWriter in Flush state");
        }

        // First read and write (into a buffer) as many items as we can;
        // then, if possible, try to flush them out.
        loop {
            match mem::replace(&mut self.state, State::Empty) {
                State::Read(flush) => {
                    match self.items.poll(tokens) {
                        Poll::Err(e) => return Poll::Err(e),
                        Poll::Ok(Some(item)) => {
                            let writer = flush.into_inner();
                            if let Some(n) = item.size_hint() {
                                self.state = State::Reserve(writer.reserve(n), item);
                            } else {
                                self.state = State::Write(item.encode(writer).into_future());
                            }
                        }
                        Poll::Ok(None) | Poll::NotReady => {
                            self.state = State::Flush(flush);
                        }
                    }
                }
                State::Reserve(mut res, item) => {
                    match res.poll(tokens) {
                        Poll::Err((e, _)) => return Poll::Err(e.into()),
                        Poll::Ok(writer) => {
                            self.state = State::Write(item.encode(writer).into_future())
                        }
                        Poll::NotReady => {
                            self.state = State::Reserve(res, item);
                            return Poll::NotReady;
                        }
                    }
                }
                State::Write(mut fut) => {
                    match fut.poll(tokens) {
                        Poll::Err(e) => return Poll::Err(e.into()),
                        Poll::Ok(writer) => self.state = State::Read(writer.flush()),
                        Poll::NotReady => {
                            self.state = State::Write(fut);
                            return Poll::NotReady;
                        }
                    }
                }
                State::Flush(mut flush) => {
                    match flush.poll(tokens) {
                        Poll::Err((e, _)) => return Poll::Err(e.into()),
                        Poll::Ok(writer) => {
                            if self.items.is_done() {
                                // Nothing more to write to sink, and no more incoming items;
                                // we're done!
                                return Poll::Ok(());
                            } else {
                                self.state = State::Read(writer.flush());
                                return Poll::NotReady;
                            }
                        }
                        Poll::NotReady => {
                            self.state = State::Read(flush);
                            return Poll::NotReady;
                        }
                    }
                }
                State::Empty => unreachable!(),
            }

            tokens = &TOKENS_ALL;
        }
    }

    fn schedule(&mut self, wake: &Arc<Wake>) {
        match self.state {
            State::Read(ref mut flush) => {
                self.items.schedule(wake);
                if flush.is_dirty() {
                    flush.schedule(wake);
                }
            }
            State::Reserve(ref mut res, _) => res.schedule(wake),
            State::Write(ref mut fut) => fut.schedule(wake),
            State::Flush(_) => unreachable!(),
            State::Empty => unreachable!(),
        }
    }
}
