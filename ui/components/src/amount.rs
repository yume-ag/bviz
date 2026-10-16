//! How an amount of file content reads on screen.

use std::ops::Div;

use bviz_invocation::spawn::{ByteMeasure, Bytes, SpawnBytes};

/// The number of each binary unit in the next, from bytes in a kibibyte.
pub const PER_BINARY_UNIT: u64 = 1 << 10;

/// The binary units past the byte, each [`PER_BINARY_UNIT`] of the one before.
const BINARY_UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// `amount` to a tenth of the largest binary unit that leaves it at least
/// one, and in bytes below a kibibyte.
pub fn bytes(amount: Bytes) -> String {
    let amount = amount.get();
    if amount < PER_BINARY_UNIT {
        return format!("{amount} B");
    }
    let per_unit = PER_BINARY_UNIT as f64;
    // A value that rounds up to a whole unit reads in that unit.
    let (value, unit) = in_binary_unit(amount as f64 / per_unit, per_unit, |value| {
        (value * 10.0).round() >= per_unit * 10.0
    });
    format!("{value:.1} {unit}")
}

/// `amount` in the largest binary unit that leaves it at least one, rounded
/// down to a whole number of it, and in bytes below a kibibyte: as a round
/// amount, such as a tick on a scale, reads.
pub fn whole_bytes(amount: Bytes) -> String {
    let amount = amount.get();
    if amount < PER_BINARY_UNIT {
        return format!("{amount} B");
    }
    let (value, unit) = in_binary_unit(amount / PER_BINARY_UNIT, PER_BINARY_UNIT, |value| {
        value >= PER_BINARY_UNIT
    });
    format!("{value} {unit}")
}

/// `kibibytes`, divided by `per_unit` into each next binary unit while
/// `fills_next` holds of it in the unit before, with the unit it ends in.
fn in_binary_unit<T: Copy + Div<Output = T>>(
    kibibytes: T,
    per_unit: T,
    fills_next: impl Fn(T) -> bool,
) -> (T, &'static str) {
    let mut value = kibibytes;
    let mut unit = 0;
    while unit + 1 < BINARY_UNITS.len() && fills_next(value) {
        value = value / per_unit;
        unit += 1;
    }
    (value, BINARY_UNITS[unit])
}

/// How a line of text names `measure`.
pub fn measure_name(measure: ByteMeasure) -> &'static str {
    match measure {
        ByteMeasure::Outputs => "bytes written",
        ByteMeasure::Inputs => "bytes read",
        ByteMeasure::Tools => "tool bytes",
    }
}

/// What spawns wrote and read, every input counted as read and the share of
/// it their tools take named only where they read any.
pub fn moved(by_spawns: SpawnBytes) -> String {
    let wrote = bytes(by_spawns.outputs);
    let read = bytes(by_spawns.inputs.all());
    match by_spawns.inputs.tools {
        Bytes::ZERO => format!("wrote {wrote} · read {read}"),
        tools => format!("wrote {wrote} · read {read} (tools {})", bytes(tools)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_invocation::spawn::InputBytes;

    #[test]
    fn an_amount_reads_in_the_largest_binary_unit_it_fills() {
        let read = |amount| bytes(Bytes::new(amount));
        assert_eq!(read(0), "0 B");
        assert_eq!(read(1023), "1023 B");
        assert_eq!(read(1024), "1.0 KiB");
        assert_eq!(read(1536), "1.5 KiB");
        assert_eq!(read(5 << 20), "5.0 MiB");
        assert_eq!(read((4 << 30) + (400 << 20)), "4.4 GiB");
        assert_eq!(read(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn an_amount_that_rounds_to_a_whole_unit_reads_in_it() {
        assert_eq!(bytes(Bytes::new((1 << 20) - 1)), "1.0 MiB");
        assert_eq!(bytes(Bytes::new((1 << 20) - 60)), "1023.9 KiB");
    }

    #[test]
    fn what_spawns_read_counts_their_tools_named_only_where_they_read_any() {
        let moved_by = |outputs, others, tools| {
            moved(SpawnBytes {
                outputs: Bytes::new(outputs),
                inputs: InputBytes {
                    tools: Bytes::new(tools),
                    others: Bytes::new(others),
                },
            })
        };
        assert_eq!(moved_by(3 << 20, 512, 0), "wrote 3.0 MiB · read 512 B");
        assert_eq!(
            moved_by(2 << 10, 3 << 20, 1 << 20),
            "wrote 2.0 KiB · read 4.0 MiB (tools 1.0 MiB)"
        );
    }

    #[test]
    fn a_round_amount_reads_as_a_whole_number_of_its_largest_unit() {
        let read = |amount| whole_bytes(Bytes::new(amount));
        assert_eq!(read(0), "0 B");
        assert_eq!(read(1 << 10), "1 KiB");
        assert_eq!(read(500 << 10), "500 KiB");
        assert_eq!(read(1 << 20), "1 MiB");
        assert_eq!(read(200 << 30), "200 GiB");
        assert_eq!(read(u64::MAX), "15 EiB");
    }
}
