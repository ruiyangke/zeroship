FROM mysql:8.4
# The shared watchdog and the blind-container refusal both check for `flock`, and
# the MySQL image does not carry util-linux.
RUN microdnf install -y util-linux && microdnf clean all
COPY watchdog.sh /usr/local/bin/zeroship-watchdog
RUN chmod 0755 /usr/local/bin/zeroship-watchdog
