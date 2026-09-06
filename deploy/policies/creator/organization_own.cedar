// Band: owner (rank >= 40).
//
// `organization:admin` appears in NO other file, so it is satisfiable only
// here. That is the design's central authority fence and it is expressed as the
// absence of a permit elsewhere rather than as a forbid: an erroring `forbid`
// does not forbid, while a band that was never written cannot be reached by any
// evaluation outcome.
//
// `organization:write` covers renaming an organization and changing its billing
// contact - identity and the party that is billed. Neither is an admin's to
// change, which is why they are not in the admin band.

permit (
  principal is User,
  action in [Action::"organization:write", Action::"organization:admin"],
  resource is Organization
) when { context.effective_rank >= 40 };
