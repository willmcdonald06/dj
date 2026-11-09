//! Tempo and phase sync between the two decks, using their beatgrids, as in Serato or
//! Rekordbox.
//!
//! - Each deck has SYNC on or off. Among synced decks one is the **master**; the
//!   other follows its tempo and beat phase.
//! - Turning SYNC on for a deck when the other deck has a track makes the other deck
//!   the master (and syncs it too); otherwise the deck becomes master itself.
//! - A follower takes its rate from the master every callback, and a small rate
//!   correction steers out phase drift, so the decks stay locked without audible
//!   jumps. Turning SYNC on while both play lines the follower up at once.
//! - Tempo changes on either synced deck move both: on a follower they are passed to
//!   the master.
//! - Turning SYNC off keeps the deck's current tempo (its fader moves there). If the
//!   master turns SYNC off, the other synced deck becomes master.
//! - Starting a following deck lands it on the master's beat.
//!
//! # RT safety
//!
//! **RT-safe**: arithmetic on the decks' state.

use crate::deck::Deck;
use crate::grid::wrap_half;

/// The phase correction aims to close an error over this long.
const PHASE_CORRECTION_SECONDS: f64 = 0.5;
/// Largest rate offset the phase correction may apply.
const MAX_PHASE_CORRECTION: f64 = 0.04;

fn other(index: usize) -> usize {
    1 - index
}

/// Which deck leads, if any deck is synced.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Sync {
    master: Option<usize>,
}

impl Sync {
    pub fn master(&self) -> Option<usize> {
        self.master
    }

    /// Whether `deck` follows the master.
    pub fn is_follower(&self, decks: &[Deck; 2], deck: usize) -> bool {
        decks[deck].sync_lock() && self.master.is_some_and(|m| m != deck)
    }

    /// Turn SYNC on or off for `deck`.
    pub fn set(&mut self, decks: &mut [Deck; 2], deck: usize, on: bool) {
        match on {
            true => self.enable(decks, deck),
            false => self.disable(decks, deck),
        }
    }

    fn enable(&mut self, decks: &mut [Deck; 2], deck: usize) {
        if decks[deck].sync_lock() {
            return;
        }
        decks[deck].set_sync_lock(true);
        let partner = other(deck);
        let master = match self.master {
            Some(master) => master,
            // Nobody leads yet: the other deck does if it has a track to follow.
            None if decks[partner].has_track() && decks[partner].grid().is_some() => {
                decks[partner].set_sync_lock(true);
                partner
            }
            None => deck,
        };
        self.master = Some(master);
        if master != deck {
            match_tempo(decks, deck);
            if decks[deck].is_moving() {
                align_phase(decks, deck);
            }
        }
    }

    fn disable(&mut self, decks: &mut [Deck; 2], deck: usize) {
        if !decks[deck].sync_lock() {
            return;
        }
        decks[deck].set_sync_lock(false);
        if self.master == Some(deck) {
            let partner = other(deck);
            self.master = match decks[partner].sync_lock() {
                true => {
                    // The follower leads now, at the tempo it was following.
                    decks[partner].become_master();
                    Some(partner)
                }
                false => None,
            };
        } else if !decks.iter().any(Deck::sync_lock) {
            self.master = None;
        }
    }

    /// A tempo fader moved on `deck`. On a follower it sets the group tempo: the
    /// master moves so the follower ends up at the rate the fader asks for.
    pub fn tempo_changed(&mut self, decks: &mut [Deck; 2], deck: usize) {
        let Some(master) = self.master else {
            return;
        };
        if master == deck || !decks[deck].sync_lock() {
            return;
        }
        let (Some(follower_grid), Some(master_grid)) = (decks[deck].grid(), decks[master].grid())
        else {
            return;
        };
        let wanted = decks[deck].fader_rate() as f64;
        let master_rate = wanted * master_grid.period / follower_grid.period;
        decks[master].set_rate_with_fader(master_rate as f32);
    }

    /// Update the follower's rate and phase correction. Call once per callback.
    pub fn update(&mut self, decks: &mut [Deck; 2], sample_rate: u32) {
        let Some(master) = self.master else {
            return;
        };
        let follower = other(master);
        if !decks[follower].sync_lock() {
            return;
        }
        let rate = matched_rate(decks, follower);
        let both_moving = decks[0].is_moving() && decks[1].is_moving();
        let correction = match (
            both_moving,
            phase_error(decks, follower),
            decks[follower].grid(),
        ) {
            (true, Some(error), Some(grid)) => {
                let frames = error * grid.period;
                (frames / (PHASE_CORRECTION_SECONDS * sample_rate as f64))
                    .clamp(-MAX_PHASE_CORRECTION, MAX_PHASE_CORRECTION) as f32
            }
            _ => 0.0,
        };
        decks[follower].set_sync_rates(rate, correction);
    }
}

/// The rate at which `follower` matches the other deck's current tempo.
fn matched_rate(decks: &[Deck; 2], follower: usize) -> Option<f32> {
    let master = &decks[other(follower)];
    let (master_grid, follower_grid) = (master.grid()?, decks[follower].grid()?);
    if !master.has_track() {
        return None;
    }
    // BPM ∝ 1 / period, so matching BPMs means scaling by the period ratio.
    Some((master.tempo_rate() as f64 * follower_grid.period / master_grid.period) as f32)
}

/// Set `follower`'s tempo to the other deck's right away (the per-callback update
/// keeps it there).
fn match_tempo(decks: &mut [Deck; 2], follower: usize) {
    let rate = matched_rate(decks, follower);
    decks[follower].set_sync_rates(rate, 0.0);
}

/// How far `follower` is behind the other deck, in beats, within ±half a beat.
/// Positive means the follower must move forward.
fn phase_error(decks: &[Deck; 2], follower: usize) -> Option<f64> {
    let master = &decks[other(follower)];
    let deck = &decks[follower];
    let master_phase = master.grid()?.phase(master.target_position());
    let follower_phase = deck.grid()?.phase(deck.target_position());
    Some(wrap_half(master_phase - follower_phase))
}

/// Jump `follower` onto the other deck's beat phase, if that deck is playing.
pub fn align_phase(decks: &mut [Deck; 2], follower: usize) {
    if !decks[other(follower)].is_moving() {
        return;
    }
    let (Some(error), Some(grid)) = (phase_error(decks, follower), decks[follower].grid()) else {
        return;
    };
    // The master keeps playing while the follower fades out to jump, so aim for where
    // its beat will be when the jump lands.
    decks[follower].shift_in_time(error * grid.period);
}
