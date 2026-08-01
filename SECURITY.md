# Security policy

Please report security issues privately to the Orkia maintainers. Do not
include private ledgers, signing keys, provider transcripts, or access tokens
in a public issue.

Never replace an existing identity key automatically. Hook adapters must keep
provider payloads immutable and redact secrets before adding diagnostic output.
