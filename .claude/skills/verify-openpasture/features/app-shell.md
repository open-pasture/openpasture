# Sign in and see the farm

A person opens the server's address in a browser. The app asks for a token (the server trusts no address). After the app token is pasted in, the map opens with the herd and its paddocks, each simulated collar is labelled "Simulated", and the status bar says "Simulated collars".

## Sub-features

- `token-prompt`: a fresh browser sees the `Token` input.
- `sign-in`: the app token opens the farm.
- `simulated-label`: the status bar says "Simulated collars" and the collar list labels each one "Simulated".

## How to get to it (user POV)

- Open `http://127.0.0.1:17990/` and paste the token that `openpasture token --data-dir <dir>` prints.
- Sign-in links (`#/signin/<code>`, `#/join/<code>`) are other ways in. They aren't driven yet.

## Driving it with bin/verify

- **Run.** `bin/verify drive app-shell`. It needs Chromium (`/usr/bin/chromium` or `$CHROME`).
- **What it does.** Headless Chromium at 1440×900 goes to `/`, waits for `getByLabel("Token")`, fills the app token, presses Enter, then waits for the text `Simulated herd` (sidebar), `Sim 1` (paddock) and `Simulated collars` (status bar).
- **Proof.** `evidence/app-shell-1-token.png` (the prompt), `evidence/app-shell-2-farm.png` (the farm, signed in) and `evidence/app-shell.json` (the final URL and any page errors). All PASS lines.

## Gotchas

- The desktop shell (Bench) doesn't print the farm's name. The phone layout does. Check the herd and paddock instead.
- Below the phone breakpoint the shell is the phone layout. Keep the 1440×900 viewport for this recipe.
- The map's imagery tiles come from the network, so a blank map in the screenshot isn't a failure. The farm name and the Simulated label are the proof.
