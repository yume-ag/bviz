//! How an amount of file content reads on screen.

use bviz_invocation::spawn::{ByteMeasure, Bytes};

const BYTES_PER_KIB: u64 = 1024;
/// The binary units past the byte, each [`BYTES_PER_KIB`] of the one before.
const BINARY_UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// `amount` to a tenth of the largest binary unit that leaves it at least
/// one, and in bytes below a kibibyte.
pub fn bytes(amount: Bytes) -> String {
    let amount = amount.get();
    if amount < BYTES_PER_KIB {
        return format!("{amount} B");
    }
    let per_unit = BYTES_PER_KIB as f64;
    let mut value = amount as f64 / per_unit;
    let mut unit = 0;
    // A value that rounds up to a whole unit reads in that unit.
    while (value * 10.0).round() >= per_unit * 10.0 && unit + 1 < BINARY_UNITS.len() {
        value /= per_unit;
        unit += 1;
    }
    format!("{value:.1} {}", BINARY_UNITS[unit])
}

/// How a line of text names `measure`.
pub fn measure_name(measure: ByteMeasure) -> &'static str {
    match measure {
        ByteMeasure::Outputs => "bytes written",
        ByteMeasure::Inputs => "bytes read",
        ByteMeasure::Tools => "tool bytes",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
