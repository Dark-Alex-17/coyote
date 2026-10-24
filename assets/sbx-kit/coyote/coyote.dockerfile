# syntax=docker/dockerfile:1
FROM darkalex17/coyote:v0.10.3
# GOOGLE_API_KEY holds the proxy-managed sentinel for sandbox tools that read it; Coyote itself reads GEMINI_API_KEY.
ENV IS_SANDBOX=1 COYOTE_LOG_LEVEL=INFO COYOTE_CONFIG_DIR=/home/agent/.config/coyote EDITOR=nano GOOGLE_API_KEY=proxy-managed
