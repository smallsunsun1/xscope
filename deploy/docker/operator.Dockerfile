FROM scratch
COPY --chown=65532:65532 .build/operator /usr/local/bin/operator
USER 65532:65532
EXPOSE 8082
ENTRYPOINT ["/usr/local/bin/operator"]
