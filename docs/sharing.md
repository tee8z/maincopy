# Share new articles

Status: article sharing is implemented. The Substack channel uses an unofficial interface and can stop working without notice.

Use this guide to post a short teaser for each newly published article to Substack and X.
Sharing is configured entirely in the admin portal. It needs no host configuration and no restart.

## What gets shared

First publication records one teaser: the article's title, its description, and its link.
The teaser always fits one X post of 280 characters. Long titles and descriptions are clipped at a word and end with `…`.
X counts every link as 23 characters and most non-Latin characters as two; the teaser uses the same rules.

Maincopy posts the teaser to every channel enabled at that moment:

| Channel | Post |
| --- | --- |
| Substack | A post titled with the article title. Its body is the description and the link. It is published to the web only; Substack emails nobody. |
| X | One post containing the title, the description, and the link. |

Edits, republishing, and restarts never share an article again.
Articles published before the upgrade to this version have no teaser, so enabling a channel shares no archive.

**Admin → Sharing** lists recent teasers with their full text. Copy that text to post it anywhere else by hand.

## Set up Substack

Substack has no publishing API. Maincopy uses the private interface of Substack's own editor, signed in as you.

1. Sign in to Substack in a browser.
2. Open the browser's developer tools and find the cookies for `substack.com`.
3. Copy the value of the `substack.sid` cookie. It starts with `s%3A`.
4. Open **Sharing** with a fresh Owner session.
5. Enter the publication address, such as `example.substack.com`, and paste the cookie.
6. Select **Enabled** and save.

The cookie grants full access to your Substack account. Treat it like a password.
It lasts some months and ends when you sign out of that browser session.
Substack's [terms of use](https://substack.com/tos) do not provide for automated publishing; decide whether this use is acceptable for your account.

Readers who subscribe on Substack receive no email from it. Point your Substack welcome text at your own signup page.

## Set up X

X charges for each post created through its API. See [X API pricing](https://docs.x.com/x-api/getting-started/pricing); a post that contains a link costs more than one without.

1. Create a project and app in the [X developer console](https://console.x.com) and add API credit.
2. Give the app **Read and write** user permissions.
3. Generate the app's API key and secret, then an access token and secret for your account.
   Regenerate the access token if you change permissions afterwards.
4. Open **Sharing** with a fresh Owner session and paste the four values.
5. Select **Enabled** and save.

These values do not expire. Replace them in the same form if you regenerate them.

## How credentials are kept

Sharing credentials are stored in the application database, not in host configuration.
The admin portal accepts them and never displays them again. Leave a credential field blank to keep the saved value.
Only an enabled Owner with a fresh session can save them, and each save is recorded in the audit log without the values.
The database file, its backups, and anyone who can read them hold these credentials.
Encrypted checkpoints protect them off the host; see [backup and restore](backup-restore.md).

## Delivery and recovery

A worker checks for queued teasers every 30 seconds and reads the current settings each time.

| State | Meaning | Action |
| --- | --- | --- |
| Queued | Waiting for the worker, a retry time, or an enabled channel. | None. Pausing a channel keeps its teasers queued. |
| Posted | The channel accepted the post. The page links to it. | None. |
| Credentials refused | The channel rejected the saved credentials. Its teasers stay queued. | Save new credentials. |
| Failed: refused | The channel rejected this post, for example as a duplicate. | Fix the cause, then select **Try again**. |
| Failed: cut off | The server stopped or lost the connection before the channel answered. The post may exist. | Check the channel first, then select **Try again** if it is missing. |
| Failed: unavailable | The channel stayed unavailable for about an hour of retries. | Select **Try again** later. |
| Failed: not understood | The channel answered in an unexpected way. Substack may have changed its interface. | Post by hand and report the problem. |

Maincopy never sends a teaser again on its own after an uncertain outcome. X rejects an exact duplicate itself.
A teaser can also be sent to a channel that was set up after the article was published: select **Share on** beside that channel.

An offline restore pauses both channels and marks unfinished teasers as cut off, because the channel may have accepted them after the backup was taken.
Review the channels, then enable sharing again.

The service journal records each delivery with its channel, article ID, and outcome. It never records credentials or teaser text.
