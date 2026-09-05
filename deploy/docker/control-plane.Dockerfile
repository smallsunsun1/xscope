FROM scratch
COPY --chown=65532:65532 .build/control-plane /usr/local/bin/control-plane
COPY --chown=65532:65532 .build/console /srv/console
USER 65532:65532
EXPOSE 8081
ENTRYPOINT ["/usr/local/bin/control-plane"]
