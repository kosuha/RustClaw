FROM node:22-slim

RUN apt-get update && apt-get install -y \
    bash \
    ca-certificates \
    git \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @openai/codex

WORKDIR /app

COPY package*.json ./
RUN npm install

COPY . ./
RUN npm run build

COPY entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh

RUN mkdir -p /workspace/group /workspace/global /workspace/extra /workspace/ipc/messages /workspace/ipc/tasks /workspace/ipc/input /workspace/ipc/responses /workspace/codex-auth
RUN chown -R node:node /workspace /app && chmod 777 /home/node

USER node
WORKDIR /workspace/group

ENTRYPOINT ["/app/entrypoint.sh"]
