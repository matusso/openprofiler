# shellcheck shell=bash
# Demo applications of the agent benchmarks (run.sh, soak.sh): build, command line, application
# packages for call counting and the URLs the load generator requests. Sourced; needs ROOT and
# DEMOS set. Apps are cloned and built with Maven in a container (Docker).
PORT="${PORT:-18080}"
PG_HOST="${PG_HOST:-127.0.0.1}"
MAVEN_IMAGE="maven:3.9-eclipse-temurin-21"
# As in java/matrix/run.sh: on Linux the user's ~/.m2, with Maven running as the user.
if [[ "$(uname -s)" == Linux ]]; then
  M2_VOLUME="${M2_VOLUME:-$HOME/.m2}"
  DOCKER_USER=(-u "$(id -u):$(id -g)")
else
  M2_VOLUME="${M2_VOLUME:-openprofiler-matrix-m2}"
  DOCKER_USER=()
fi

mvn_build() { # dir, args...
  local dir="$1"; shift
  docker run --rm "${DOCKER_USER[@]+"${DOCKER_USER[@]}"}" -v "$M2_VOLUME:/var/maven/.m2" -e MAVEN_CONFIG=/var/maven/.m2 \
    -v "$dir:/w" -w /w "$MAVEN_IMAGE" mvn -B -q -Duser.home=/var/maven package -DskipTests "$@"
}

# ---- apps ----------------------------------------------------------------------------------------

petclinic_build() {
  local d="$DEMOS/spring-petclinic"
  [[ -d "$d" ]] || git clone -q --depth 1 https://github.com/spring-projects/spring-petclinic.git "$d"
  compgen -G "$d/target/spring-petclinic-*.jar" >/dev/null || mvn_build "$d" -Dcheckstyle.skip -Dspring-javaformat.skip=true
}
petclinic_cmd() {
  echo "-Xmx1g -jar $(ls "$DEMOS"/spring-petclinic/target/spring-petclinic-*.jar | head -1) --server.port=$PORT"
}
petclinic_instrument() { echo "org.springframework.samples.petclinic.*"; }
petclinic_urls() {
  local b="http://127.0.0.1:$PORT"
  for i in 1 2 3 4 5 6 7 8 9 10; do echo "$b/owners/$i"; done
  echo "$b/owners?lastName=Davis" "$b/owners?page=1" "$b/vets.html" "$b/owners/find"
}

quarkus_build() {
  local d="$DEMOS/quarkus-quickstarts"
  [[ -d "$d" ]] || git clone -q --depth 1 https://github.com/quarkusio/quarkus-quickstarts.git "$d"
  [[ -f "$d/hibernate-orm-panache-quickstart/target/quarkus-app/quarkus-run.jar" ]] || mvn_build "$d/hibernate-orm-panache-quickstart"
  if command -v docker >/dev/null && ! docker inspect opmatrix-postgres >/dev/null 2>&1; then
    docker run -d --name opmatrix-postgres -p 55432:5432 -e POSTGRES_PASSWORD=matrix -e POSTGRES_USER=matrix \
      -e POSTGRES_DB=matrix postgres:16-alpine >/dev/null
    for _ in $(seq 1 60); do docker exec opmatrix-postgres pg_isready -U matrix >/dev/null 2>&1 && break; sleep 1; done
  fi
}
quarkus_cmd() {
  echo "-Xmx1g -Dquarkus.http.port=$PORT -Dquarkus.datasource.jdbc.url=jdbc:postgresql://$PG_HOST:55432/matrix" \
    "-Dquarkus.datasource.username=matrix -Dquarkus.datasource.password=matrix" \
    "-Dquarkus.hibernate-orm.schema-management.strategy=drop-and-create -Dquarkus.hibernate-orm.log.sql=false" \
    "-jar $DEMOS/quarkus-quickstarts/hibernate-orm-panache-quickstart/target/quarkus-app/quarkus-run.jar"
}
quarkus_instrument() { echo "org.acme.*"; }
quarkus_urls() {
  local b="http://127.0.0.1:$PORT"
  echo "$b/entity/fruits" "$b/entity/fruits/1" "$b/entity/fruits/2" "$b/repository/fruits" "$b/repository/fruits/3"
}
