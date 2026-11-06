## attachments

Hand a session a file, or read one it was given:

```bash
remuda instance attach worker-a ./notes.md           # prints {"objectId": "obj_…", …}
remuda instance send worker-a --attach obj_… "Read [File #1] first."
remuda instance attachment obj_… --out ./copy.md      # the bytes back
```

From a seat this works only for sessions you own. Another session's file
answers 404, the same as one that does not exist; a staged file expires
after a day and has the Hub's size limit.
