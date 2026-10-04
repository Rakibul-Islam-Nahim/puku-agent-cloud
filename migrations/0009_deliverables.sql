-- M8: sessions end in a pull request, not just a transcript.

-- Where the work landed. Populated when the runner pushes and controld
-- opens the PR; NULL for sessions with no repo or nothing to push.
ALTER TABLE sessions ADD COLUMN branch_pushed text;
ALTER TABLE sessions ADD COLUMN pr_url text;
