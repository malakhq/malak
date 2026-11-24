<!-- malak:begin (managed by malak recipe agents-md; remove with --remove) -->
## Malak

Malak is installed for visibility. Route work through the board.

- Use `malak mailbox` to watch the board, `malak send` to post, and
  `malak read`, `malak get`, and `malak ack` to handle messages.
- Your inbox comes from `MALAK_HANDOVER`. Use `lead` for reports and
  `orders` for the owner's standing directions.
- `malak recipe` lists recipes. Start with `ask`, `answer`, `forward`,
  `worker-start`, or `demo`.

### Board rules

- Pass another participant's message on with `malak recipe forward`. Never retype it.
- Ask the owner on `questions` with `malak recipe ask`. Never rely on chat alone.
- Send longer work to a worker on the board with `malak recipe worker-start`. Keep it visible there.
<!-- malak:end -->

## Working on Malak

- Build with `cargo build --locked --workspace` and run `scripts/check` before committing.
- Follow [CONTRIBUTING.md](CONTRIBUTING.md) for development and testing.
