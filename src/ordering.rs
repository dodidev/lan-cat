use std::{cmp::Ordering, collections::BTreeMap};

use serde::{Deserialize, Serialize};

pub type DeviceId = String;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct VersionVector(pub BTreeMap<DeviceId, u64>);

impl VersionVector {
    pub fn increment(&mut self, device: &str) -> u64 {
        let value = self.0.entry(device.to_owned()).or_default();
        *value += 1;
        *value
    }

    pub fn merge(&mut self, other: &Self) {
        for (id, value) in &other.0 {
            let local = self.0.entry(id.clone()).or_default();
            *local = (*local).max(*value);
        }
    }

    pub fn relation(&self, other: &Self) -> ClockRelation {
        let mut less = false;
        let mut greater = false;
        for key in self.0.keys().chain(other.0.keys()) {
            match self
                .0
                .get(key)
                .unwrap_or(&0)
                .cmp(other.0.get(key).unwrap_or(&0))
            {
                Ordering::Less => less = true,
                Ordering::Greater => greater = true,
                Ordering::Equal => {}
            }
        }
        match (less, greater) {
            (true, true) => ClockRelation::Concurrent,
            (true, false) => ClockRelation::Before,
            (false, true) => ClockRelation::After,
            (false, false) => ClockRelation::Equal,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ClockRelation {
    Before,
    After,
    Concurrent,
    Equal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_relations_and_merge() {
        let mut a = VersionVector::default();
        a.increment("a");
        let mut b = a.clone();
        b.increment("b");
        assert_eq!(a.relation(&b), ClockRelation::Before);
        a.increment("a");
        assert_eq!(a.relation(&b), ClockRelation::Concurrent);
        a.merge(&b);
        assert_eq!(a.0, BTreeMap::from([("a".into(), 2), ("b".into(), 1)]));
    }
}
