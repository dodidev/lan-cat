use std::fmt;

use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize, ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }
}

impl fmt::Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.to_possible_value().expect("edge value").get_name()
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum PointerInput {
    Motion { dx: f64, dy: f64 },
    Button { button: u32, state: u32 },
    Axis { axis: u8, value: f64 },
    AxisDiscrete120 { axis: u8, value: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyboardInput {
    pub key: u32,
    pub state: u32,
}

impl KeyboardInput {
    pub fn validate(self) -> Result<()> {
        if self.key > 0x2ff || self.state > 1 {
            bail!("invalid keyboard input")
        }
        Ok(())
    }
}

impl PointerInput {
    pub fn validate(self) -> Result<()> {
        match self {
            Self::Motion { dx, dy }
                if !dx.is_finite()
                    || !dy.is_finite()
                    || dx.abs() > 10_000.0
                    || dy.abs() > 10_000.0 =>
            {
                bail!("invalid pointer motion")
            }
            Self::Axis { value, .. } if !value.is_finite() || value.abs() > 10_000.0 => {
                bail!("invalid pointer axis")
            }
            Self::Button { state, .. } if state > 1 => bail!("invalid pointer button state"),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum InputMessage {
    Probe {
        edge: Edge,
        position: f64,
        progress: f32,
    },
    ProbeAck,
    Cancel,
    Enter {
        edge: Edge,
        position: f64,
    },
    Ack,
    Leave,
    Pointer(PointerInput),
    Keyboard(KeyboardInput),
    Ping,
    Pong,
}

impl InputMessage {
    pub fn validate(self) -> Result<()> {
        match self {
            Self::Pointer(event) => event.validate()?,
            Self::Keyboard(event) => event.validate()?,
            Self::Probe {
                position, progress, ..
            } if !position.is_finite()
                || !(0.0..=1.0).contains(&position)
                || !progress.is_finite()
                || !(0.0..=1.0).contains(&progress) =>
            {
                bail!("invalid cursor probe")
            }
            Self::Enter { position, .. }
                if !position.is_finite() || !(0.0..=1.0).contains(&position) =>
            {
                bail!("invalid cursor entry")
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_are_opposites() {
        assert_eq!(Edge::Left.opposite(), Edge::Right);
        assert_eq!(Edge::Top.opposite(), Edge::Bottom);
    }

    #[test]
    fn validates_pointer_values() {
        assert!(
            PointerInput::Motion { dx: 2.0, dy: -1.0 }
                .validate()
                .is_ok()
        );
        assert!(
            PointerInput::Motion {
                dx: f64::NAN,
                dy: 0.0
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn validates_keyboard_values() {
        assert!(KeyboardInput { key: 30, state: 1 }.validate().is_ok());
        assert!(KeyboardInput { key: 30, state: 2 }.validate().is_err());
        assert!(
            KeyboardInput {
                key: 0x300,
                state: 1
            }
            .validate()
            .is_err()
        );
    }
}
