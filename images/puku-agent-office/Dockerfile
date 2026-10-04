# puku-agent-office: the guest image for document work.
#
# The lean puku-agent image can still produce a PDF or a deck — the skills
# tell the agent to pip-install reportlab/python-pptx into a venv, and the
# egress allowlist permits pypi. That costs a minute of the user's time on
# every session and cannot convert or render anything LibreOffice-grade.
#
# This variant bakes the same stack puku-chat-compute-service already runs,
# so a document session starts working immediately. It is roughly twice the
# size of the lean image, which is why it is a variant selected per session
# rather than the default.
ARG BASE=puku-agent:latest
FROM ${BASE}

USER root

# Matches Dockerfile.office-sandbox in puku-chat-compute-service. fonts are
# not optional: without them matplotlib and LibreOffice silently render
# boxes instead of glyphs, and nobody notices until the PDF is delivered.
RUN apt-get update && apt-get install -y --no-install-recommends \
      libreoffice-writer libreoffice-calc libreoffice-impress \
      pandoc \
      fonts-dejavu fonts-liberation \
      poppler-utils qpdf \
      python3-pip python-is-python3 \
      libpango-1.0-0 libpangoft2-1.0-0 libharfbuzz0b libcairo2 libgdk-pixbuf-2.0-0 \
 && rm -rf /var/lib/apt/lists/*

# weasyprint renders HTML with pango/cairo through cffi at import time, so
# the shared objects above are a hard requirement, not an optimisation:
# without them `import weasyprint` raises OSError and the HTML->PDF path in
# the pdf skill is dead. Caught by actually importing it in the built image.

# Ubuntu 24.04 is PEP 668, so a system pip install needs the override. This
# is a container image, not a shared machine — the whole filesystem is the
# environment.
RUN pip3 install --no-cache-dir --break-system-packages \
      python-pptx python-docx openpyxl reportlab pypdf pdfplumber \
      markdown weasyprint matplotlib pandas

# The npm side of the pptx skill.
RUN npm install -g pptxgenjs docx

# NODE_PATH is not optional, even though the packages above are installed.
# node does not search the global root, so `require('pptxgenjs')` from
# /workspace fails MODULE_NOT_FOUND -- and the obvious workaround, an inline
# `NODE_PATH=/usr/lib/node_modules node build.js`, is refused by puku-cli's
# Bash sandbox because the value is a path outside /workspace. That leaves the
# agent with an installed module it cannot reach and no legal way to point at
# it: observed burning 35 of 61 turns before escaping via `cp -r`, which hit
# the turn ceiling with the report unwritten.
#
# Setting it here means `require` just works and no command needs a prefix.
ENV NODE_PATH=/usr/lib/node_modules

ENV SKILLS_ROOT=/session/home/.puku-cli/skills

USER agent
WORKDIR /workspace
