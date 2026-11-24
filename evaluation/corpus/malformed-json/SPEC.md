# Service: corpus-malformed-json

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `POST /parse`
- Rules: Parse a JSON object and reject malformed JSON.
- Response `200`: Parsed.
- Response `400`: Invalid JSON.

## Verification
### `POST /parse`
- Body: {bad
- Response `400`
- Body contains: invalid JSON
