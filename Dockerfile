FROM debian:bullseye-slim 

RUN apt-get update -y && apt-get install -yq ca-certificates curl

COPY main/resources/docker/configure.sh /usr/local/bin/configure.sh
COPY main/resources/docker/entrypoint.sh /usr/local/bin/entrypoint.sh

RUN sed -i -e 's/__C__/jmap/g' /usr/local/bin/configure.sh && \
    sed -i -e 's/__R__/jmap-server/g' /usr/local/bin/configure.sh && \
    sed -i -e 's/__N__/jmap-sqlite/g' /usr/local/bin/configure.sh && \
    sed -i -e 's/__B__/stalwart-jmap/g' /usr/local/bin/entrypoint.sh

RUN chmod a+rx /usr/local/bin/*.sh

RUN /usr/local/bin/configure.sh --download

RUN useradd stalwart-mail -s /sbin/nologin -M
RUN mkdir -p /opt/stalwart-mail
RUN chown stalwart-mail:stalwart-mail /opt/stalwart-mail

VOLUME [ "/opt/stalwart-mail" ]

EXPOSE	8080 25 587 465 143 993 4190

ENTRYPOINT ["/bin/sh", "/usr/local/bin/entrypoint.sh"]
