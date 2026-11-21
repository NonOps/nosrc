# Service: Minimal Example

A minimal service specification for the nosrc JavaScript runtime.

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /hello`
- Rules: Return a JSON greeting.
- Response `200`: `{ "message": "hello" }`

## Examples

A representative request and response:

- Request: `GET /hello`
- Response body: `{ "message": "hello" }`

These examples are illustrative; the operator-controlled verification cases below remain the acceptance policy.

## Verification
### `GET /hello`
- Response `200`
- Body contains: `hello`
