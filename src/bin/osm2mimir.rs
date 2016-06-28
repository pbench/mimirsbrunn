// Copyright © 2016, Canal TP and/or its affiliates. All rights reserved.
//
// This file is part of Navitia,
//     the software to build cool stuff with public transport.
//
// Hope you'll enjoy and contribute to this project,
//     powered by Canal TP (www.canaltp.fr).
// Help us simplify mobility and open public transport:
//     a non ending quest to the responsive locomotion way of traveling!
//
// LICENCE: This program is free software; you can redistribute it
// and/or modify it under the terms of the GNU Affero General Public
// License as published by the Free Software Foundation, either
// version 3 of the License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful, but
// WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
// Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public
// License along with this program. If not, see
// <http://www.gnu.org/licenses/>.
//
// Stay tuned using
// twitter @navitia
// IRC #navitia on freenode
// https://groups.google.com/d/forum/navitia
// www.navitia.io

#[macro_use]
extern crate log;
extern crate osmpbfreader;
extern crate rustc_serialize;
extern crate docopt;
extern crate mimir;
extern crate rs_es;
extern crate chrono;

extern crate mimirsbrunn;
extern crate serde;
extern crate geo;

#[macro_use]
extern crate mdo;

use std::collections::{BTreeSet, BTreeMap};
use mimir::rubber::Rubber;

pub type AdminsVec = Vec<Rc<mimir::Admin>>;
pub type StreetsVec = Vec<mimir::Street>;
pub type OsmPbfReader = osmpbfreader::OsmPbfReader<std::fs::File>;
pub type StreetWithRelationSet = BTreeSet<osmpbfreader::OsmId>;
pub type AdminSet = BTreeSet<Rc<mimir::Admin>>;
pub type NameAdminMap = BTreeMap<StreetKey, Vec<osmpbfreader::OsmId>>;

use mimirsbrunn::admin_geofinder::AdminGeoFinder;
use std::rc::Rc;
use std::cell::Cell;

#[derive(RustcDecodable, Debug)]
struct Args {
    flag_input: String,
    flag_level: Vec<u32>,
    flag_city_level: u32,
    flag_connection_string: String,
    flag_import_way: bool,
    flag_import_admin: bool,
    flag_dataset: String,
}

static USAGE: &'static str = "
Usage:
    osm2mimir --help
    osm2mimir --input=<file> [--connection-string=<connection-string>] [--import-way] [--import-admin] [--dataset=<dataset>] [--city-level=<level>] --level=<level> ...

Options:
    -h, --help              Show this message.
    -i, --input=<file>      OSM PBF file.
    -l, --level=<level>     Admin levels to keep.
    -C, --city-level=<level>
                            City level to calculate weight, [default: 8] 		
    -w, --import-way        Import ways
    -a, --import-admin      Import admins
    -c, --connection-string=<connection-string>
                            Elasticsearch parameters, [default: http://localhost:9200/munin]
    -d, --dataset=<dataset>
                            Name of the dataset, [default: fr]
    
";

#[derive(Debug)]
struct AdminMatcher {
    admin_levels: BTreeSet<u32>,
}
impl AdminMatcher {
    pub fn new(levels: BTreeSet<u32>) -> AdminMatcher {
        AdminMatcher { admin_levels: levels }
    }

    pub fn is_admin(&self, obj: &osmpbfreader::OsmObj) -> bool {
        match *obj {
            osmpbfreader::OsmObj::Relation(ref rel) => {
                rel.tags.get("boundary").map_or(false, |v| v == "administrative") &&
                rel.tags.get("admin_level").map_or(false, |lvl| {
                    self.admin_levels.contains(&lvl.parse::<u32>().unwrap_or(0))
                })
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct StreetKey {
    pub name: String,
    pub admins: AdminSet,
}

fn parse_osm_pbf(path: &str) -> OsmPbfReader {
    let path = std::path::Path::new(&path);
    osmpbfreader::OsmPbfReader::new(std::fs::File::open(&path).unwrap())
}

fn administrative_regions(pbf: &mut OsmPbfReader, levels: BTreeSet<u32>) -> AdminsVec {
    let mut administrative_regions = AdminsVec::new();
    let matcher = AdminMatcher::new(levels);
    info!("reading pbf...");
    let objects = osmpbfreader::get_objs_and_deps(pbf, |o| matcher.is_admin(o)).unwrap();
    info!("reading pbf done.");
    // load administratives regions
    for (_, obj) in &objects {
        if !matcher.is_admin(&obj) {
            continue;
        }
        if let &osmpbfreader::OsmObj::Relation(ref relation) = obj {
            let level = relation.tags
                                .get("admin_level")
                                .and_then(|s| s.parse().ok());
            let level = match level {
                None => {
                    info!("invalid admin_level for relation {}: admin_level {:?}",
                          relation.id,
                          relation.tags.get("admin_level"));
                    continue;
                }
                Some(l) => l,
            };
            // administrative region with name ?
            let name = match relation.tags.get("name") {
                Some(val) => val,
                None => {
                    warn!("adminstrative region without name for relation {}:  admin_level {} \
                           ignored.",
                          relation.id,
                          level);
                    continue;
                }
            };

            // admininstrative region without coordinates
            let coord_centre = relation.refs
                                       .iter()
                                       .find(|rf| rf.role == "admin_centre")
                                       .and_then(|r| {
                                           objects.get(&r.member).and_then(|value| {
                                               match value {
                                                   &osmpbfreader::OsmObj::Node(ref node) => {
                                                       Some(mimir::CoordWrapper(geo::Coordinate {
                                                           x: node.lat,
                                                           y: node.lon,
                                                       }))
                                                   }
                                                   _ => None,
                                               }
                                           })
                                       });
            let (admin_id, insee_id) = match relation.tags.get("ref:INSEE") {
                Some(val) => {
                    (format!("admin:fr:{}", val.trim_left_matches('0')), val.trim_left_matches('0'))
                }
                None => (format!("admin:osm:{}", relation.id), ""),
            };
            let zip_code = match relation.tags.get("addr:postcode") {
                Some(val) => &val[..],
                None => match relation.tags.get("postal_code") {
                    Some(val) => &val[..],
                    None => ""
                },
            };
            let boundary = mimirsbrunn::boundaries::build_boundary(&relation, &objects);
            let admin = mimir::Admin {
                id: admin_id,
                insee: insee_id.to_string(),
                level: level,
                label: name.to_string(),
                zip_codes: zip_code.split(';').map(|s| s.to_string()).collect(),
                // TODO weight value ?
                weight: Cell::new(0),
                coord: coord_centre,
                boundary: boundary,
            };
            administrative_regions.push(Rc::new(admin));
        }
    }
    return administrative_regions;
}

fn make_admin_geofinder(admins: &AdminsVec) -> AdminGeoFinder {
    let mut geofinder = AdminGeoFinder::new();

    for a in admins {
        geofinder.insert(a.clone());
    }
    geofinder
}

fn get_street_admin(admins_geofinder: &AdminGeoFinder,
                    obj_map: &BTreeMap<osmpbfreader::OsmId, osmpbfreader::OsmObj>,
                    way: &osmpbfreader::objects::Way)
                    -> Vec<Rc<mimir::Admin>> {
    // for the moment we consider that the coord of the way is the coord of it's first node
    let coord = way.nodes
                   .iter()
                   .filter_map(|node_id| obj_map.get(&osmpbfreader::OsmId::Node(*node_id)))
                   .filter_map(|node_obj| {
                       if let &osmpbfreader::OsmObj::Node(ref node) = node_obj {
                           Some(geo::Coordinate {
                               x: node.lat,
                               y: node.lon,
                           })
                       } else {
                           None
                       }
                   })
                   .next();
    coord.map_or(vec![], |c| admins_geofinder.get(&c))
}

fn format_label(admins: &AdminsVec, city_level: u32, name: &str) -> String {
    match admins.iter().position(|adm| adm.level == city_level) {
        Some(idx) => format!("{} ({})", name, admins[idx].label),
        None => name.to_string()
    }
}
fn get_zip_codes_for_street(admins: &AdminsVec) -> Vec<String>{
    let level = admins.iter().fold(0, |mut level, adm| {
            if adm.level > level && !adm.zip_codes.is_empty() {
                level=adm.level
            }; 
            level
    });
    let mut zip_codes = Vec::new();
	if level > 0 {
	    // Many admins with adm.level == level
        for adm in admins {
            if adm.level == level {
                zip_codes.extend(adm.zip_codes.clone());
            }
        }
	}
    zip_codes
}
fn streets(pbf: &mut OsmPbfReader, admins: &AdminsVec, city_level: u32) -> StreetsVec {
    let admins_geofinder = make_admin_geofinder(admins);

    let is_valid_obj = |obj: &osmpbfreader::OsmObj| -> bool {
        match *obj {
            osmpbfreader::OsmObj::Way(ref way) => {
                way.tags.get("highway").map_or(false, |x| !x.is_empty()) &&
                way.tags.get("name").map_or(false, |x| !x.is_empty())
            }
            osmpbfreader::OsmObj::Relation(ref rel) => {
                rel.tags.get("type").map_or(false, |v| v == "associatedStreet")
            }
            _ => false,
        }
    };
    info!("reading pbf...");
    let objs_map = osmpbfreader::get_objs_and_deps(pbf, is_valid_obj).unwrap();
    info!("reading pbf done.");
    let mut street_rel: StreetWithRelationSet = BTreeSet::new();
    let mut street_list: StreetsVec = vec![];
    // Sometimes, streets can be devided into several "way"s that still have the same street name.
    // The reason why a street is devided may be that a part of the street become a bridge/tunne/etc.
    // In this case, a "relation" tagged with (type = associatedStreet) is used to group all these "way"s.
    // In order not to have duplicates in autocompleion,
    // we should tag the osm ways in the relation not to index them twice.

    for (_, rel_obj) in &objs_map {
        if let &osmpbfreader::OsmObj::Relation(ref rel) = rel_obj {
            let way_name = rel.tags.get("name");
            for ref_obj in &rel.refs {

                use mdo::option::*;
                let objs_map = &objs_map;
                let street_list = &mut street_list;
                let admins_geofinder = &admins_geofinder;

                let inserted = mdo! {
                    when ref_obj.member.is_way();
                    when ref_obj.role == "street";
                    obj =<< objs_map.get(&ref_obj.member);
                    way =<<obj.way();
                    way_name =<< way_name.or_else(|| way.tags.get("name"));
                    let admin = get_street_admin(admins_geofinder, objs_map, way);
                    ret ret(street_list.push(mimir::Street {
                                id: way.id.to_string(),
                                street_name: way_name.to_string(),
                                label: format_label(&admin, city_level, way_name),
                                weight: 1,
                                zip_codes: get_zip_codes_for_street(&admin),
                                administrative_regions: admin,
                    }))
                };
                if inserted.is_some() {
                    break;
                }
            }

            // Add osmid of all the relation members in de set
            // We don't create any street for all the osmid present in street_rel
            for ref_obj in &rel.refs {
                if ref_obj.member.is_way() {
                    street_rel.insert(ref_obj.member);
                }
            }
        };
    }

    // we merge all the ways with a key = way_name + admin list of level(=city_level)
    // we use a map NameAdminMap <key, value> to manage the merging of ways
    let mut name_admin_map: NameAdminMap = BTreeMap::new();
    for (osmid, obj) in &objs_map {
        if street_rel.contains(osmid) {
            continue;
        }
        use mdo::option::*;
        let admins_geofinder = &admins_geofinder;
        let objs_map = &objs_map;
        let name_admin_map = &mut name_admin_map;
        mdo! {
            way =<< obj.way();
            let admins: BTreeSet<Rc<mimir::Admin>> = get_street_admin(admins_geofinder, objs_map, way)
            	.into_iter()
            	.filter(|admin| admin.level == city_level)
            	.collect();

            way_name =<< way.tags.get("name");
            let key = StreetKey{name: way_name.to_string(), admins: admins};
            ret ret(name_admin_map.entry(key).or_insert(vec![]).push(*osmid))
        };
    }

    // Create a street for each way with osmid present in in objs_map
    for (_, way_ids) in name_admin_map {
        use mdo::option::*;
        let objs_map = &objs_map;
        let street_list = &mut street_list;
        let admins_geofinder = &admins_geofinder;
        mdo! {
            obj =<< objs_map.get(&way_ids[0]);
            way =<< obj.way();
            way_name =<< way.tags.get("name");
            let admins = get_street_admin(admins_geofinder, objs_map, way);
            ret ret(street_list.push(mimir::Street{
   	                    id: way.id.to_string(),
   	                    street_name: way_name.to_string(),
   	                    label: format_label(&admins, city_level, way_name),
   	                    weight: 1,
   	                    zip_codes: get_zip_codes_for_street(&admins),
   	                    administrative_regions: admins,
            }))
        };
    }

    street_list
}


fn main() {
    mimir::logger_init().unwrap();
    let args: Args = docopt::Docopt::new(USAGE)
                         .and_then(|d| d.decode())
                         .unwrap_or_else(|e| e.exit());

    let levels = args.flag_level.iter().cloned().collect();
    let city_level = args.flag_city_level;
    let mut parsed_pbf = parse_osm_pbf(&args.flag_input);
    debug!("creation of indexes");
    let mut rubber = Rubber::new(&args.flag_connection_string);

    info!("creating adminstrative regions");
    let admins = administrative_regions(&mut parsed_pbf, levels);

    info!("computing city weight");
    let mut streets = streets(&mut parsed_pbf, &admins, city_level);

    for st in &mut streets {
        for admin in &mut st.administrative_regions {
            admin.weight.set(admin.weight.get() + 1)
        }
    }

    for st in &mut streets {
        for admin in &mut st.administrative_regions {
            if admin.level == city_level {
                st.weight = admin.weight.get();
                break;
            }
        }
    }

    if args.flag_import_way {
        info!("importing streets into Mimir");
        let nb_streets = rubber.index("way", &args.flag_dataset, streets.into_iter())
                               .unwrap();
        info!("Nb of indexed street: {}", nb_streets);
    }

    let nb_admins = rubber.index("admin", &args.flag_dataset, admins.iter())
                          .unwrap();
    info!("Nb of indexed admin: {}", nb_admins);

}
