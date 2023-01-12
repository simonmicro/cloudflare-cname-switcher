FROM debian:stable-slim
ENV DEBIAN_FRONTEND=noninteractive

# Setup/Libs
RUN apt-get update && apt-get install -y python3 python3-pip curl && rm -rf /var/lib/apt/lists/*
RUN pip3 install ipgetter2

# Setup/Script
WORKDIR /workdir
COPY cname_switcher.py .

# Install the healthcheck
HEALTHCHECK --start-period=10s --interval=60s CMD curl -f http://localhost/healthz || exit 1
# Expose port for /healthz path
EXPOSE 80

# Command
CMD python3 -u cname_switcher.py