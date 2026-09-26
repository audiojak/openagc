# Use your own Google OAuth client

OpenAGC signs in to Gmail with an OAuth "Desktop app" client and ships
with the project's own. Until Google verifies that client it is in
*Testing* status, so only listed test users can sign in with it; everyone
else can create a client in their own Google Cloud project. It takes about
five minutes, and your mail still goes only from Google to your Mac.

## 1. Create a project and enable the Gmail API

1. Open the [Google Cloud console](https://console.cloud.google.com/) and
   create a project (any name, e.g. "OpenAGC").
2. Go to **APIs & Services → Library**, search for **Gmail API**, and click
   **Enable**.

## 2. Configure the consent screen

1. Go to **Google Auth Platform → Branding** (older consoles: **APIs &
   Services → OAuth consent screen**).
2. Enter an app name (e.g. "OpenAGC (personal)") and your email as the
   support and developer contact.
3. **Audience**: choose **External** for a personal Gmail account, or
   **Internal** if you use Google Workspace and only need your own domain.
4. For External apps in **Testing** status, add your Gmail address under
   **Test users**.
5. Under **Data access**, add the scopes
   `https://www.googleapis.com/auth/gmail.modify`, `openid` and
   `.../auth/userinfo.profile`. The last two are non-sensitive; they give
   OpenAGC your name and picture for the account switcher.

## 3. Create the client

1. Go to **Clients → Create client**.
2. **Application type: Desktop app**. Name it anything.
3. Copy the **Client ID** and **Client secret**.

Desktop clients use a loopback redirect (`http://127.0.0.1:<port>`), which
Google allows automatically; there is nothing to configure.

## 4. Connect in OpenAGC

1. In OpenAGC's welcome screen (or **Settings → Accounts**), open
   **Advanced: use your own Google OAuth client**.
2. Paste the client ID and secret and click **Save**. The secret is stored
   in your Mac's Keychain.
3. Click **Connect Gmail**. Your browser opens Google's sign-in page.
4. Google shows **"Google hasn't verified this app"** because the client is
   yours and unverified. Click **Continue**: you are the developer.
5. Allow access. The browser says you can close the window, and OpenAGC
   starts syncing, inbox first.

## Things to know

- **Weekly sign-in in Testing status.** Google expires refresh tokens after
  7 days for apps in *Testing* that use Gmail scopes. OpenAGC will show
  "Gmail needs you to sign in again". To avoid it, set the app's publishing
  status to *In production* (for a personal, unverified client this keeps
  the warning screen but ends the weekly expiry).
- **Why `gmail.modify`?** It lets OpenAGC read mail and change labels
  (archive, read/unread) and send. It cannot permanently delete mail.
  Besides it, OpenAGC asks only for `openid profile` (your name and
  picture).
- **The client secret is not really secret.** Google treats desktop clients
  as unable to keep secrets; security comes from PKCE and the loopback
  redirect, not the secret. OpenAGC still keeps it out of logs.
- **Revoking access**: [myaccount.google.com/permissions](https://myaccount.google.com/permissions).
