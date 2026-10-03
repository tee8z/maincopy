# Share new articles

Status: article sharing is implemented. The Substack channel uses an unofficial interface and can stop working without notice.

Use this guide to post a short teaser for each newly published article to Substack, and to copy the same teaser for X or anywhere else.
Sharing is configured entirely in the admin portal. It needs no host configuration and no restart.

## What gets shared

First publication records one teaser: a post text made of the article's title and description, and the article's link.
Together they always fit one X post of 280 characters. Long titles and descriptions are clipped at a word and end with `…`.
X counts every link as 23 characters and most non-Latin characters as two; the teaser uses the same rules.

When the Substack channel is enabled at that moment, Maincopy publishes the teaser there:
a post titled with the article title, whose body is the description and the link.
It is published to the web only; Substack emails nobody.

Edits, republishing, and restarts never share an article again.
Articles published before the upgrade to this version have no teaser, so enabling a channel shares no archive.

## Post on X or elsewhere by hand

**Admin → Sharing** lists recent teasers with their post text and link in separate fields.
Copy the post text into a new post, then add the link under it or in a reply.
Maincopy does not use the X API and stores no X credentials.

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

## How credentials are kept

The Substack session cookie is stored in the application database, not in host configuration.
The admin portal accepts it and never displays it again. Leave the field blank to keep the saved value.
Only an enabled Owner with a fresh session can save it, and each save is recorded in the audit log without the value.
The database file, its backups, and anyone who can read them hold this credential.
Encrypted checkpoints protect it off the host; see [backup and restore](backup-restore.md).

## Delivery and recovery

A worker checks for queued teasers every 30 seconds and reads the current settings each time.

| State | Meaning | Action |
| --- | --- | --- |
| Queued | Waiting for the worker, a retry time, or an enabled channel. | None. Pausing Substack keeps its teasers queued. |
| Posted | The channel accepted the post. The page links to it. | None. |
| Credentials refused | Substack rejected the saved cookie. Its teasers stay queued. | Save a new cookie. |
| Failed: refused | Substack rejected this post. | Fix the cause, then select **Try again**. |
| Failed: cut off | The server stopped before Substack answered. The post may exist. | Check Substack first, then select **Try again** if it is missing. |
| Failed: unavailable | Substack stayed unavailable for about an hour of retries. | Select **Try again** later. |
| Failed: not understood | Substack answered in an unexpected way. It may have changed its interface. | Post by hand and report the problem. |

Maincopy never sends a teaser again on its own after an uncertain outcome.
A teaser can also be sent to Substack when it was set up after the article was published: select **Share on Substack**.

An offline restore pauses Substack and marks unfinished teasers as cut off, because Substack may have accepted them after the backup was taken.
Review the publication, then enable sharing again.

The service journal records each delivery with its channel, article ID, and outcome. It never records the cookie or teaser text.
