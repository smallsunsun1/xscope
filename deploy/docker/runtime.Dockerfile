FROM mirror.gcr.io/library/python:3.13-slim
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1
WORKDIR /app
COPY python/pyproject.toml ./
COPY python/src ./src
RUN pip install --no-cache-dir . && useradd --system --uid 65532 --home /nonexistent xscope
USER 65532:65532
EXPOSE 8090
ENTRYPOINT ["xscope-runtime"]
