# Service: corpus-query-parameters

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /search`
- Rules: Return the decoded q query parameter.
- Response `200`: JSON object containing term.

## Verification
### `GET /search?q=ada`
- Response `200`
- Body contains: "term":"ada"
