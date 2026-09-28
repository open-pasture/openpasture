# Texting and alerts

Alerts, approvals by text and the morning brief are in the free app. Bring your own Twilio number
and they cost only what Twilio charges. The hosted relay is the paid way to skip Twilio.

All of this is in Settings, as the owner.

## Your own Twilio number

1. In Twilio, buy a number that can send SMS. In the US, register it before it sends: A2P 10DLC
   for a local number, toll-free verification for a toll-free one. Until then Twilio refuses the
   texts, and openpasture shows Twilio's reason beside each one (Data > Messages).
2. Settings > Texting > Twilio: Account SID, Auth token and the SMS number (`+15155550100`, or a
   Messaging Service SID `MG…`). Save, then Test: without a number it checks the account, with one
   it sends "openpasture test.".
3. Settings > People: add each person with a phone and a role. Someone who only texts needs no
   sign-in. Verify texts them a 6-digit code; they text it back, or you type it in. Nobody gets a
   text, or can text a command, until their phone is verified.
4. Each person picks what reaches them in You > Alerts (severity, herds, quiet hours, on duty).

### How replies come in

Replies (Y, N, OK, questions) reach the server one of two ways. Settings > Texting shows which.

- **Webhook**, when the server has a public https URL (Settings > Server > Public URL). Settings
  > Texting shows "Replies by webhook" and the URL to copy. In Twilio: Phone Numbers > your number
  > Messaging > "A message comes in": Webhook, that URL, HTTP POST. Every request is checked
  against Twilio's signature over the public URL.
- **Polling**, with no public URL (a farm behind NAT). Nothing to set up in Twilio: the server
  reads the number's messages from Twilio every 10 s ("Replies checked every 10 s").

### WhatsApp

Add a WhatsApp sender in Twilio and its number here. Alerts and the brief start a conversation, so
WhatsApp needs an approved template with one `{{1}}` body variable (its `HX…` SID in WhatsApp
template SID). Without one, WhatsApp only answers people who texted in the last 24 hours.

## What people can text

| Text | Who | Does |
| --- | --- | --- |
| `Y` / `N` | manager, owner | approves or rejects what the last text asked about; 12 h after the farm last texted that phone, add the code from the text (`Y 4821`) |
| `LATER` | manager, owner | asks again in an hour |
| `OK` | hand and up | acks the alerts in the last alert text |
| `STATUS` | anyone | each herd: head, paddock, moves, open alerts, what waits |
| `WHERE 214` | anyone | the animal's last position in words, its age and a map link |
| `STOP MOVE` | manager, owner | stops the running move where it is |
| `STOP` / `START` | anyone | stops or restarts texts to that phone |
| anything else | anyone | a question, answered in one text by the farm's brain (an API key or Claude Code; otherwise the list of texts) |

A breakout of the whole herd is one text per person, not one per animal.

## Email

Settings > Texting > Email: mail server, port (587), user, password, from address and encryption
(STARTTLS, TLS, or none for a server on the farm's own network). Email carries alerts and the
brief; replies by email aren't read.

## Webhook

Settings > Texting > Webhook: a URL and a signing secret. Every alert is posted once as JSON with
`x-openpasture-signature: t=<unix>,v1=<hex>`, an HMAC-SHA256 of `"<t>.<body>"` with the secret:

```
printf '%s.%s' "$t" "$body" | openssl dgst -sha256 -hmac "$secret"
```

## Web Push

Free, with no Twilio: on a phone that opened the app over https, You > Alerts on this phone. On an
iPhone, add the app to the Home Screen first (iOS 16.4+).

## The morning brief

Settings > Daily > Brief and a time (farm time). Turn it on per person in People. Each herd's
brief is one text of at most 480 characters; one that asks takes a bare Y or N.

## The hosted relay

For a farm without its own Twilio: texts go out from openpasture's number.

1. Settings > Texting > Relay: the relay key from your openpasture subscription (the URL defaults
   to `https://api.openpasture.dev`). Check "Send through the relay"; it turns on once the relay
   accepts the key.
2. Verify each phone again (Settings > People > Verify): the relay sends its own code, since it
   only texts numbers proven to it.
3. Replies come back through the relay; nothing to set up.

Worth knowing:

- The relay texts SMS only. Email needs your own mail server.
- STOP to the relay's number stops texts from every farm on it, as Twilio blocks the number for
  that sender. START undoes it.
- A bare Y when two farms asked the same person gets a reply asking for the code.
- If your server stops checking in for 15 minutes, the relay texts your owners and managers once.

### Running your own relay

Any openpasture server can relay for other farms from its own Twilio: Settings > Hosting > New key
for each farm, then check Relay texts. The farm puts this server's https URL and its key in Relay. Each key is limited to 30 texts a minute and
500 a day, and texts only phones verified for it.
