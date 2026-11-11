# ShareCraft

Website: [share.thinecraft.io](https://share.thinecraft.io)

To build and run the current development build, and for what it does and does not do, see [BUILDING.html](BUILDING.html). It needs a PostgreSQL database (configured through `.env`, copied from `.env.example`). The four file editors, where each fork comes from, and what has been checked are described in [editors.html](editors.html).

ShareCraft is the planned open-source SharePoint alternative in the ThineCraft family. Its purpose is to provide a shared workspace for organizations to create intranet sites, team portals, knowledge bases, and other collaborative content experiences.

## Planned scope

ShareCraft is intended to bring together the core capabilities commonly associated with SharePoint:

- Team sites, communication sites, intranets, and departmental portals
- Document libraries with folders, metadata, version history, and collaborative access
- Real-time co-authoring and presence support for WordCraft, GridCraft, PdfCraft, SolidCraft, DeckCraft, and other compatible ThineCraft applications
- Lists for structured information, lightweight tracking, and shared operational records
- Pages, news, navigation, and reusable content for publishing internal information
- Search and discovery across sites, documents, lists, and people-related content
- Permissions, sharing controls, auditability, retention, and governance
- Workflow and integration points for processes built with other ThineCraft projects
- A platform on which specialized services can be built without defining ShareCraft around any one service

ShareCraft may support services such as ThineWedding, ThineMemorial, and ThineCapture, but those are applications built on top of the platform rather than ShareCraft's primary purpose.

> **Status:** Early development build. It is not a release and is not a SharePoint replacement yet. What exists today: people sign in with accounts an administrator creates; plain-text documents are shared per document as viewer, editor or owner; several people edit one document together in the browser; someone whose access is removed is put out of the live session at once; content is saved as Git revisions, and accounts, sharing, a revision index and an event log are kept in PostgreSQL. Spreadsheets, word documents, PDFs and 3D designs open in GridCraft, WordCraft, PdfCraft and SolidCraft, built from ThineCraft's forks, and save back as Git revisions, one person editing at a time. The server listens on the local machine only, and the sites, libraries, lists, search and live co-authoring in the four editors described above are not built. [BUILDING.html](BUILDING.html) lists the limits in full.
