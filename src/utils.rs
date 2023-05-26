pub fn wrap_compare_less(lhs: u16, rhs: u16) -> bool {
    let dist_down = lhs.wrapping_sub(rhs);
    let dist_up = rhs.wrapping_sub(lhs);
    dist_down < dist_up
}