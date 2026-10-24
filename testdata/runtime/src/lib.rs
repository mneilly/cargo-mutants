use std::ops::Add;

pub fn add<T: Add<Output = T>>(left: T, right: T) -> T {
    left + right
}

pub fn nested(value: u32) -> u32 {
    fn increment(value: u32) -> u32 {
        value + 1
    }
    increment(value)
}

pub const fn constant() -> u32 {
    7
}

pub fn greet() -> &'static str {
    "hello"
}

pub fn flag() -> bool {
    true
}

pub fn push(values: &mut Vec<u32>) {
    values.push(7);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions_produce_expected_results() {
        assert_eq!(add(2_u32, 3), 5);
        assert_eq!(nested(2), 3);
        assert_eq!(constant(), 7);
        assert_eq!(greet(), "hello");
        assert!(flag());
        let mut values = Vec::new();
        push(&mut values);
        assert_eq!(values, [7]);
    }
}
