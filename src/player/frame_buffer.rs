use rodio::{Sample, Source};
use rtp_rs::Seq;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) struct FrameBuffer<S> {
    data: BTreeMap<Seq, std::vec::IntoIter<S>>,

    read_marker: Seq,
    write_marker: Seq,
}

impl<S> FrameBuffer<S>
where
    S: Sample,
{
    /// Builds a new `FrameBuffer`.
    pub(crate) fn new(initial_seq: Seq) -> FrameBuffer<S> {
        FrameBuffer {
            data: BTreeMap::new(),
            read_marker: initial_seq,
            write_marker: initial_seq,
        }
    }

    fn pop_front(&mut self) -> Option<std::vec::IntoIter<S>> {
        // trace!("packet popped");
        let data = self.data.remove(&self.read_marker);
        self.read_marker = self.read_marker.next();
        data
    }

    pub(crate) fn add_packet(&mut self, seq: Seq, packet: std::vec::IntoIter<S>) {
        // trace!("packet added with seq {:?}", seq);
        self.write_marker = seq;
        self.data.insert(seq, packet);
    }
}

pub(crate) struct FrameBufferSource<S> {
    frame_buffer: Arc<Mutex<FrameBuffer<S>>>,
    channels: u16,
    sample_rate: u32,

    current: Option<std::vec::IntoIter<S>>,
}

impl<S> FrameBufferSource<S>
where
    S: Sample,
{
    /// Builds a new `FrameBufferSource`.
    ///
    /// # Panic
    ///
    /// - Panics if the number of channels is zero.
    /// - Panics if the samples rate is zero.
    ///
    pub(crate) fn new(
        frame_buffer: Arc<Mutex<FrameBuffer<S>>>,
        channels: u16,
        sample_rate: u32,
    ) -> FrameBufferSource<S> {
        assert!(channels != 0);
        assert!(sample_rate != 0);

        FrameBufferSource {
            frame_buffer,
            channels,
            sample_rate,

            current: None,
        }
    }
}

impl<S> Source for FrameBufferSource<S>
where
    S: Sample,
{
    #[inline]
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    #[inline]
    fn channels(&self) -> u16 {
        self.channels
    }

    #[inline]
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

impl<S> Iterator for FrameBufferSource<S>
where
    S: Sample,
{
    type Item = S;

    #[inline]
    fn next(&mut self) -> Option<S> {
        // TODO cleanup
        if let Some(mut current) = self.current.take() {
            if current.len() > 0 {
                let val = current.next();
                self.current = Some(current);
                return val;
            }
        }

        let mut data = self.frame_buffer.lock().unwrap();
        self.current = data.pop_front();
        drop(data);

        if let Some(mut current) = self.current.take() {
            if current.len() > 0 {
                let val = current.next();
                self.current = Some(current);
                return val;
            }
        }

        Some(S::zero_value())
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}
