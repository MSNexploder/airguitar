use nom::{
    branch::permutation,
    bytes::complete::tag_no_case,
    character::complete::{char, digit1, space0},
    combinator::{map_res, opt},
    sequence::{delimited, preceded, terminated, tuple},
    IResult,
};

#[derive(Debug, Clone)]
pub(crate) struct RtpInfo {
    /// Sequence number of the first packet that is a direct result of the request.
    pub seq: u16,
    /// RTP timestamp corresponding to the start time.
    pub rtptime: u32,
}

impl RtpInfo {
    pub(crate) fn parse(input: &str) -> IResult<&str, RtpInfo> {
        let (input, (seq, rtptime)) = permutation((
            parameter(tag_no_case("seq")),
            parameter(tag_no_case("rtptime")),
        ))(input)?;
        Ok((input, RtpInfo { seq, rtptime }))
    }
}

fn parameter<'a, O1, O2, E, F>(seq_parser: F) -> impl FnMut(&'a str) -> IResult<&'a str, O2, E>
where
    F: nom::Parser<&'a str, O1, E>,
    O2: std::str::FromStr,
    E: nom::error::ParseError<&'a str> + nom::error::FromExternalError<&'a str, <O2>::Err>,
{
    terminated(
        preceded(
            tuple((trim(seq_parser), char('='))),
            trim(map_res(digit1, |s: &str| s.parse::<O2>())),
        ),
        opt(char(';')),
    )
}

fn trim<I, O, E: nom::error::ParseError<I>, F>(parser: F) -> impl FnMut(I) -> IResult<I, O, E>
where
    F: nom::Parser<I, O, E>,
    I: nom::InputTakeAtPosition,
    <I as nom::InputTakeAtPosition>::Item: nom::AsChar + Clone,
{
    delimited(space0, parser, space0)
}
