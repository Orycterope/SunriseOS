fn main() {
    let data: &[u8] = &[0; 10];
    let _: &[i8] = data.into();
    //~^ ERROR the trait bound `&[i8]: std::convert::From<&[u8]>` is not satisfied
}
