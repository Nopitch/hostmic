//! The lock-free protocol between the stream's state changes and `ADVANCE` (`host::Cursor`),
//! replayed on a model of the driver's atomics with any interleaving of host requests and
//! state changes, including a state change between an `ADVANCE`'s check and its
//! compare-and-exchange.
//!
//! Checked after every step: frames are accepted only while the stream runs and only for the
//! current generation; the position is the sum of the frames accepted since the stream last
//! stopped; every state change changes the word, so an `ADVANCE` checked before it can never
//! commit after it.

#![no_main]

use arbitrary::Arbitrary;
use hostmic_abi::AdvanceRequest;
use hostmic_driver_core::host::{self, Cursor};
use libfuzzer_sys::fuzz_target;

/// A state change, as PortCls drives it (serialized among themselves).
#[derive(Arbitrary, Debug, Clone, Copy)]
enum Change {
    Run,
    Pause,
    Stop,
    /// A new stream buffer of this many frames (the stream is halted first).
    Buffer(u16),
}

#[derive(Arbitrary, Debug)]
enum Step {
    Change(Change),
    /// The host reads `GET_STREAM` and remembers the generation and position.
    Refresh,
    /// The host advances from what it last read.
    Advance(u32),
    /// Any request at all.
    Raw {
        generation: u32,
        frames: u32,
        from: u64,
    },
    /// An `ADVANCE` checked against the word, then a state change, then its
    /// compare-and-exchange.
    Race(u32, Change),
}

/// The driver's atomics, and what the model expects of them.
struct Model {
    word: u64,
    buffer_frames: u32,
    /// Frames accepted since the stream last stopped.
    accepted: u64,
    /// What the host last read.
    host: Cursor,
}

impl Model {
    /// Applies a state change the way the protocol prescribes: halt the generation before
    /// changing anything else.
    fn change(&mut self, change: Change) {
        let before = self.word;
        let cursor = Cursor::unpack(self.word);
        let cursor = match change {
            Change::Run => cursor.running(),
            Change::Pause => cursor.halted(),
            Change::Stop => {
                self.accepted = 0;
                cursor.stopped()
            }
            Change::Buffer(frames) => {
                let halted = cursor.halted();
                self.word = halted.pack();
                self.buffer_frames = u32::from(frames);
                halted
            }
        };
        self.word = cursor.pack();
        assert_ne!(self.word, before, "a state change left the word unchanged");
    }

    /// `ADVANCE`: check against the loaded word, then compare-and-exchange; `between` runs
    /// between the two.
    fn advance(&mut self, request: &AdvanceRequest, between: Option<Change>) {
        let loaded = self.word;
        let result = host::advance(&request.to_bytes(), loaded, self.buffer_frames);
        if let Some(change) = between {
            self.change(change);
        }
        let Ok(new) = result else {
            return;
        };
        let cursor = Cursor::unpack(loaded);
        assert!(cursor.is_running());
        assert_eq!(request.generation, u32::from(cursor.generation));
        if self.word != loaded {
            // The exchange fails; the driver refuses the request.
            return;
        }
        self.word = new;
        self.accepted += u64::from(request.frames);
        self.host = Cursor::unpack(new);
    }

    fn check(&self) {
        assert_eq!(Cursor::unpack(self.word).position, self.accepted);
    }
}

fuzz_target!(|steps: Vec<Step>| {
    let mut model = Model {
        word: Cursor::STOPPED.pack(),
        buffer_frames: 0,
        accepted: 0,
        host: Cursor::STOPPED,
    };
    for step in steps {
        match step {
            Step::Change(change) => model.change(change),
            Step::Refresh => model.host = Cursor::unpack(model.word),
            Step::Advance(frames) => {
                let request = AdvanceRequest {
                    generation: u32::from(model.host.generation),
                    frames,
                    from: model.host.position,
                };
                model.advance(&request, None);
            }
            Step::Raw {
                generation,
                frames,
                from,
            } => model.advance(
                &AdvanceRequest {
                    generation,
                    frames,
                    from,
                },
                None,
            ),
            Step::Race(frames, change) => {
                let cursor = Cursor::unpack(model.word);
                let request = AdvanceRequest {
                    generation: u32::from(cursor.generation),
                    frames,
                    from: cursor.position,
                };
                model.advance(&request, Some(change));
            }
        }
        model.check();
    }
});
